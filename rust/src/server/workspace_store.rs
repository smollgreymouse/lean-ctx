// SPDX-License-Identifier: Apache-2.0
//! Launcher-bound persisted Workspace. MCP requests cannot select a backing file.
//! A stable lock file serializes readers/writers across processes; writes replace
//! the JSON atomically and are never performed inside an allowed project root.
use fs2::FileExt;
use serde::{Deserialize, Serialize};
#[cfg(not(unix))]
use std::fs;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::Path,
};

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredCheckoutBinding {
    pub repository_key: String,
    pub checkout_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_generation: Option<String>,
    pub git_common_dir: String,
    pub git_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredWorkspace {
    pub id: String,
    pub revision: u64,
    pub primary: String,
    pub additional: Vec<String>,
}

/// New lifecycle metadata lives beside the legacy Workspace JSON so old
/// long-lived LeanCTX processes can continue reading the store during a rolling
/// upgrade. The sidecar is protected by the *same* store lock and fenced by
/// Workspace id + revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredWorkspaceMetadata {
    workspace_id: String,
    revision: u64,
    #[serde(default)]
    checkout_bindings: std::collections::BTreeMap<String, StoredCheckoutBinding>,
}

/// Crash-consistent sidecar envelope. During a Workspace mutation the sidecar
/// temporarily carries both the currently committed metadata and the next
/// revision. Therefore either side of the legacy JSON atomic rename has an
/// exact identity fence available. Old single-record sidecars are still read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredWorkspaceMetadataJournal {
    schema_version: u32,
    versions: Vec<StoredWorkspaceMetadata>,
}

const WORKSPACE_METADATA_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceAdminView {
    pub id: String,
    pub revision: u64,
    pub primary: String,
    pub additional: Vec<String>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub checkout_bindings: std::collections::BTreeMap<String, StoredCheckoutBinding>,
    pub primary_status: WorkspaceRootStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_statuses: Vec<WorkspaceRootStatus>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkspaceRootState {
    Active,
    Moved,
    Missing,
    Unavailable,
    Unsafe,
    IdentityMismatch,
    SiblingWorktree,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceRootStatus {
    pub configured: String,
    pub active: Option<std::path::PathBuf>,
    pub state: WorkspaceRootState,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedWorkspace {
    pub configured: StoredWorkspace,
    pub checkout_bindings: std::collections::BTreeMap<String, StoredCheckoutBinding>,
    pub primary: WorkspaceRootStatus,
    pub additional: Vec<WorkspaceRootStatus>,
    pub authority_fingerprint: String,
}

impl ResolvedWorkspace {
    pub(crate) fn active_primary(&self) -> Option<&std::path::Path> {
        self.primary.active.as_deref()
    }

    pub(crate) fn active_additional(&self) -> Vec<std::path::PathBuf> {
        let mut roots: Vec<_> = self
            .additional
            .iter()
            .filter_map(|status| status.active.clone())
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }
}

#[cfg(any(debug_assertions, test, not(unix)))]
fn metadata_path(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| std::ffi::OsString::from("workspace"));
    name.push(".meta");
    path.with_file_name(name)
}

fn validate_store_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.is_symlink() {
        return Err("Workspace store must have an absolute, non-symlink path".into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_options() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    opts
}

fn verify_private_file(file: &std::fs::File) -> Result<(), String> {
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("Workspace state must be a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid reads the effective process UID and takes no pointers.
        if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
            return Err("Workspace state must be owned by the process user and mode 0600".into());
        }
    }
    Ok(())
}

#[cfg(unix)]
struct StoreAnchor {
    dir: std::fs::File,
    stable_parent: PathBuf,
    name: std::ffi::OsString,
}

#[cfg(unix)]
impl StoreAnchor {
    fn open(path: &Path) -> Result<Self, String> {
        validate_store_path(path)?;
        let parent = path.parent().ok_or("Missing parent directory")?;
        let stable_parent = parent.canonicalize().map_err(|e| e.to_string())?;
        // Reject an already-symlinked/aliased parent; after this point the opened
        // dirfd is the authority anchor and survives rename/symlink swaps safely.
        if stable_parent != parent {
            return Err("Workspace parent directory must be canonical and non-symlinked".into());
        }
        let mut opts = OpenOptions::new();
        opts.read(true);
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let dir = opts.open(&stable_parent).map_err(|e| e.to_string())?;
        if !dir.metadata().map_err(|e| e.to_string())?.is_dir() {
            return Err("Workspace parent is not a directory".into());
        }
        #[cfg(debug_assertions)]
        crate::server::workspace_test_hooks::external_hit("store_before_open");
        let name = path
            .file_name()
            .ok_or("Workspace file name missing")?
            .to_os_string();
        Ok(Self {
            dir,
            stable_parent,
            name,
        })
    }

    fn c_name(name: &std::ffi::OsStr) -> Result<std::ffi::CString, String> {
        std::ffi::CString::new(name.as_bytes())
            .map_err(|_| "Workspace filename contains NUL".into())
    }

    fn open_named(
        &self,
        name: &std::ffi::OsStr,
        flags: i32,
        mode: libc::mode_t,
    ) -> Result<std::fs::File, String> {
        let c = Self::c_name(name)?;
        // SAFETY: self.dir owns a live directory fd and c is a live NUL-terminated
        // CString for the duration of openat; flags/mode are valid libc values.
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                c.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: the successful openat yielded a fresh owned fd; this call
        // transfers ownership exactly once to File.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        Ok(file)
    }

    fn open_store(&self) -> Result<std::fs::File, String> {
        let file = self.open_named(&self.name, libc::O_RDWR, 0)?;
        verify_private_file(&file)?;
        Ok(file)
    }

    fn open_optional_named(
        &self,
        name: &std::ffi::OsStr,
        flags: i32,
    ) -> Result<Option<std::fs::File>, String> {
        let c = Self::c_name(name)?;
        // SAFETY: self.dir owns a live directory fd and c is a live NUL-terminated
        // CString for the duration of openat; flags/mode are valid libc values.
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                c.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error.to_string());
        }
        // SAFETY: the successful openat yielded a fresh owned fd; this call
        // transfers ownership exactly once to File.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        verify_private_file(&file)?;
        Ok(Some(file))
    }

    fn lock_name(&self) -> std::ffi::OsString {
        let mut name = self.name.clone();
        name.push(".lock");
        name
    }

    fn open_lock(&self) -> Result<std::fs::File, String> {
        let name = self.lock_name();
        let file = self.open_named(&name, libc::O_RDWR | libc::O_CREAT, 0o600)?;
        verify_private_file(&file)?;
        Ok(file)
    }

    fn create_store(&self) -> Result<std::fs::File, String> {
        let file = self.open_named(
            &self.name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        verify_private_file(&file)?;
        Ok(file)
    }

    fn store_display_path(&self) -> PathBuf {
        self.stable_parent.join(&self.name)
    }

    fn metadata_name(&self) -> std::ffi::OsString {
        let mut name = self.name.clone();
        name.push(".meta");
        name
    }

    fn temporary_name_for(&self, target: &std::ffi::OsStr) -> std::ffi::OsString {
        let mut name = std::ffi::OsString::from(".");
        name.push(target);
        name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
        name
    }

    fn temporary_name(&self) -> std::ffi::OsString {
        self.temporary_name_for(&self.name)
    }

    fn rename_within(&self, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> Result<(), String> {
        let from = Self::c_name(from)?;
        let to = Self::c_name(to)?;
        // SAFETY: both directory fds are held by self.dir and the two names
        // are NUL-terminated CStrings alive throughout renameat.
        let rc = unsafe {
            libc::renameat(
                self.dir.as_raw_fd(),
                from.as_ptr(),
                self.dir.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    fn unlink(&self, name: &std::ffi::OsStr) {
        if let Ok(name) = Self::c_name(name) {
            // SAFETY: self.dir owns the live directory fd and name is a
            // NUL-terminated CString alive for the duration of unlinkat.
            unsafe {
                libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0);
            }
        }
    }

    fn sync_dir(&self) -> Result<(), String> {
        self.dir.sync_all().map_err(|e| e.to_string())
    }
}

#[cfg(unix)]
fn read_unlocked(anchor: &StoreAnchor) -> Result<StoredWorkspace, String> {
    let file = anchor.open_store()?;
    let mut data = String::new();
    (&file)
        .read_to_string(&mut data)
        .map_err(|e| e.to_string())?;
    let value: StoredWorkspace = serde_json::from_str(&data).map_err(|e| e.to_string())?;
    validate_value(&anchor.store_display_path(), value)
}

#[cfg(not(unix))]
fn read_unlocked(path: &Path) -> Result<StoredWorkspace, String> {
    validate_store_path(path)?;
    let file = private_options().open(path).map_err(|e| e.to_string())?;
    verify_private_file(&file)?;
    let mut data = String::new();
    (&file)
        .read_to_string(&mut data)
        .map_err(|e| e.to_string())?;
    let value: StoredWorkspace = serde_json::from_str(&data).map_err(|e| e.to_string())?;
    validate_value(path, value)
}

fn decode_metadata_records(data: &str) -> Result<Vec<StoredWorkspaceMetadata>, String> {
    if let Ok(journal) = serde_json::from_str::<StoredWorkspaceMetadataJournal>(data) {
        if journal.schema_version != WORKSPACE_METADATA_SCHEMA_VERSION {
            return Err(format!(
                "unsupported Workspace metadata schema {}",
                journal.schema_version
            ));
        }
        if journal.versions.is_empty() || journal.versions.len() > 2 {
            return Err("Workspace metadata journal must contain one or two revisions".into());
        }
        return Ok(journal.versions);
    }
    serde_json::from_str::<StoredWorkspaceMetadata>(data)
        .map(|metadata| vec![metadata])
        .map_err(|e| format!("invalid Workspace metadata: {e}"))
}

fn select_metadata_for_value(
    value: &StoredWorkspace,
    records: Vec<StoredWorkspaceMetadata>,
) -> Result<StoredWorkspaceMetadata, String> {
    records
        .into_iter()
        .find(|metadata| metadata.workspace_id == value.id && metadata.revision == value.revision)
        .ok_or_else(|| {
            format!(
                "Workspace metadata/store fence mismatch: no metadata for {}@{}",
                value.id, value.revision
            )
        })
        .and_then(|metadata| validate_metadata(value, metadata))
}

#[cfg(unix)]
fn read_metadata_raw_unlocked(
    anchor: &StoreAnchor,
) -> Result<Option<Vec<StoredWorkspaceMetadata>>, String> {
    let name = anchor.metadata_name();
    let Some(file) = anchor.open_optional_named(&name, libc::O_RDONLY)? else {
        return Ok(None);
    };
    let mut data = String::new();
    (&file)
        .read_to_string(&mut data)
        .map_err(|e| e.to_string())?;
    decode_metadata_records(&data).map(Some)
}

#[cfg(unix)]
fn read_metadata_unlocked(
    anchor: &StoreAnchor,
    value: &StoredWorkspace,
) -> Result<Option<StoredWorkspaceMetadata>, String> {
    read_metadata_raw_unlocked(anchor)?
        .map(|records| select_metadata_for_value(value, records))
        .transpose()
}

#[cfg(not(unix))]
fn read_metadata_raw_unlocked(path: &Path) -> Result<Option<Vec<StoredWorkspaceMetadata>>, String> {
    let path = metadata_path(path);
    let file = match private_options().open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    verify_private_file(&file)?;
    let mut data = String::new();
    (&file)
        .read_to_string(&mut data)
        .map_err(|e| e.to_string())?;
    decode_metadata_records(&data).map(Some)
}

#[cfg(not(unix))]
fn read_metadata_unlocked(
    path: &Path,
    value: &StoredWorkspace,
) -> Result<Option<StoredWorkspaceMetadata>, String> {
    read_metadata_raw_unlocked(path)?
        .map(|records| select_metadata_for_value(value, records))
        .transpose()
}

fn validate_value(store_path: &Path, value: StoredWorkspace) -> Result<StoredWorkspace, String> {
    if value.revision == 0 || uuid::Uuid::parse_str(&value.id).is_err() {
        return Err("Invalid Workspace identity or revision".into());
    }
    validate_configured_root(&value.primary)?;
    for root in &value.additional {
        validate_configured_root(root)?;
        if root == &value.primary {
            return Err("Invalid or duplicate Workspace root".into());
        }
    }
    if value.additional.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("Workspace roots must be sorted and unique".into());
    }
    reject_store_in_roots(store_path, &value)?;
    Ok(value)
}

fn validate_metadata(
    value: &StoredWorkspace,
    metadata: StoredWorkspaceMetadata,
) -> Result<StoredWorkspaceMetadata, String> {
    if metadata.workspace_id != value.id || metadata.revision != value.revision {
        return Err(format!(
            "Workspace metadata/store fence mismatch: metadata {}@{}, store {}@{}",
            metadata.workspace_id, metadata.revision, value.id, value.revision
        ));
    }
    for (root, binding) in &metadata.checkout_bindings {
        if root != &value.primary && !value.additional.contains(root) {
            return Err("Workspace checkout binding references an unknown root".into());
        }
        validate_configured_root(root)?;
        for (label, metadata_path) in [
            ("git common dir", binding.git_common_dir.as_str()),
            ("git dir", binding.git_dir.as_str()),
        ] {
            let path = Path::new(metadata_path);
            if !path.is_absolute()
                || path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::CurDir | std::path::Component::ParentDir
                    )
                })
            {
                return Err(format!(
                    "Workspace {label} binding is not absolute/normalized"
                ));
            }
        }
        for (label, key) in [
            ("repository", binding.repository_key.as_str()),
            ("checkout", binding.checkout_key.as_str()),
        ] {
            if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!("Workspace {label} key is invalid"));
            }
        }
    }
    Ok(metadata)
}

/// Validate the persisted binding without requiring the directory to exist.
///
/// A Workspace store is configuration, not a live filesystem snapshot. A root
/// that was canonical when it was registered may legitimately disappear after
/// `git worktree remove`, an unmount, or another external lifecycle event. The
/// store must remain readable so callers can diagnose or revoke that binding.
fn validate_configured_root(root: &str) -> Result<(), String> {
    let path = Path::new(root);
    if !path.is_absolute() {
        return Err("Workspace root must be absolute".into());
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err("Workspace root must be lexically normalized".into());
    }
    if crate::core::pathutil::is_broad_or_unsafe_root(path) {
        return Err("Workspace root is unsafe".into());
    }
    Ok(())
}

pub(crate) fn validate_root(root: &str) -> Result<String, String> {
    // Keep Windows long/short and verbatim path spellings consistent with Git checkout discovery.
    let canonical =
        crate::core::pathutil::canonicalize_raw(Path::new(root)).map_err(|e| e.to_string())?;
    if !canonical.is_dir() || crate::core::pathutil::is_broad_or_unsafe_root(&canonical) {
        return Err("Workspace root is unavailable or unsafe".into());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn binding_for_root(root: &str) -> Option<StoredCheckoutBinding> {
    let checkout = crate::core::git_checkout::discover_checkout(Path::new(root))
        .ok()
        .flatten()?;
    Some(StoredCheckoutBinding {
        repository_key: checkout.repository.repository_key,
        checkout_key: checkout.checkout_key,
        checkout_generation: checkout.checkout_generation,
        git_common_dir: checkout
            .repository
            .git_common_dir
            .to_string_lossy()
            .into_owned(),
        git_dir: checkout.git_dir.to_string_lossy().into_owned(),
    })
}

fn derive_live_bindings(
    value: &StoredWorkspace,
) -> std::collections::BTreeMap<String, StoredCheckoutBinding> {
    std::iter::once(&value.primary)
        .chain(value.additional.iter())
        .filter_map(|root| binding_for_root(root).map(|binding| (root.clone(), binding)))
        .collect()
}

fn metadata_for(
    value: &StoredWorkspace,
    checkout_bindings: std::collections::BTreeMap<String, StoredCheckoutBinding>,
) -> StoredWorkspaceMetadata {
    StoredWorkspaceMetadata {
        workspace_id: value.id.clone(),
        revision: value.revision,
        checkout_bindings,
    }
}

fn checkout_matches_binding(
    checkout: &crate::core::git_checkout::CheckoutIdentity,
    binding: &StoredCheckoutBinding,
) -> bool {
    checkout.repository.repository_key == binding.repository_key
        && checkout.checkout_key == binding.checkout_key
        && checkout.repository.git_common_dir.to_string_lossy() == binding.git_common_dir
        && checkout.git_dir.to_string_lossy() == binding.git_dir
        && binding
            .checkout_generation
            .as_ref()
            .is_none_or(|generation| checkout.checkout_generation.as_ref() == Some(generation))
}

fn resolve_root(root: &str, binding: Option<&StoredCheckoutBinding>) -> WorkspaceRootStatus {
    let path = Path::new(root);
    match path.canonicalize() {
        Ok(canonical)
            if canonical.is_dir()
                && !crate::core::pathutil::is_broad_or_unsafe_root(&canonical)
                && canonical.to_string_lossy() == root =>
        {
            if let Some(binding) = binding {
                match crate::core::git_checkout::discover_checkout(&canonical) {
                    Ok(Some(checkout)) if checkout_matches_binding(&checkout, binding) => {
                        WorkspaceRootStatus {
                            configured: root.to_string(),
                            active: Some(canonical),
                            state: WorkspaceRootState::Active,
                        }
                    }
                    Ok(_) => WorkspaceRootStatus {
                        configured: root.to_string(),
                        active: None,
                        state: WorkspaceRootState::IdentityMismatch,
                    },
                    Err(_) => WorkspaceRootStatus {
                        configured: root.to_string(),
                        active: None,
                        state: WorkspaceRootState::Unavailable,
                    },
                }
            } else {
                WorkspaceRootStatus {
                    configured: root.to_string(),
                    active: Some(canonical),
                    state: WorkspaceRootState::Active,
                }
            }
        }
        Ok(_) => WorkspaceRootStatus {
            configured: root.to_string(),
            active: None,
            state: WorkspaceRootState::Unsafe,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(binding) = binding
                && let Ok(Some(checkout)) = crate::core::git_checkout::resolve_checkout_by_git_dir(
                    Path::new(&binding.git_dir),
                )
                && checkout_matches_binding(&checkout, binding)
            {
                return WorkspaceRootStatus {
                    configured: root.to_string(),
                    active: Some(checkout.root),
                    state: WorkspaceRootState::Moved,
                };
            }
            WorkspaceRootStatus {
                configured: root.to_string(),
                active: None,
                state: WorkspaceRootState::Missing,
            }
        }
        Err(_) => WorkspaceRootStatus {
            configured: root.to_string(),
            active: None,
            state: WorkspaceRootState::Unavailable,
        },
    }
}

fn authority_fingerprint(
    configured: &StoredWorkspace,
    primary: &WorkspaceRootStatus,
    additional: &[WorkspaceRootStatus],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(configured.id.as_bytes());
    hasher.update(&configured.revision.to_le_bytes());
    hasher.update(&[0]);

    if let Some(root) = primary.active.as_deref() {
        hasher.update(root.to_string_lossy().as_bytes());
        if let Ok(Some(checkout)) = crate::core::git_checkout::discover_checkout(root) {
            hasher.update(checkout.repository.repository_key.as_bytes());
            hasher.update(checkout.checkout_key.as_bytes());
            if let Some(generation) = checkout.checkout_generation.as_deref() {
                hasher.update(generation.as_bytes());
            }
        }
    } else {
        hasher.update(b"<primary-unavailable>");
    }
    hasher.update(&[0]);

    for status in additional {
        if let Some(root) = status.active.as_deref() {
            hasher.update(root.to_string_lossy().as_bytes());
            if let Ok(Some(checkout)) = crate::core::git_checkout::discover_checkout(root) {
                hasher.update(checkout.repository.repository_key.as_bytes());
                hasher.update(checkout.checkout_key.as_bytes());
                if let Some(generation) = checkout.checkout_generation.as_deref() {
                    hasher.update(generation.as_bytes());
                }
            }
            hasher.update(&[0]);
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn suppress_sibling_worktree_authority(
    primary: &WorkspaceRootStatus,
    additional: &mut [WorkspaceRootStatus],
) {
    let primary_checkout = primary.active.as_deref().and_then(|root| {
        crate::core::git_checkout::discover_checkout(root)
            .ok()
            .flatten()
    });

    if let Some(primary_checkout) = primary_checkout.as_ref() {
        for status in additional.iter_mut() {
            let Some(root) = status.active.as_deref() else {
                continue;
            };
            let Ok(Some(checkout)) = crate::core::git_checkout::discover_checkout(root) else {
                continue;
            };
            if crate::core::git_checkout::same_repository(primary_checkout, &checkout)
                && primary_checkout.checkout_key != checkout.checkout_key
            {
                status.active = None;
                status.state = WorkspaceRootState::SiblingWorktree;
            }
        }
    }

    let mut by_repository: std::collections::BTreeMap<
        String,
        Vec<(usize, String, Option<String>)>,
    > = std::collections::BTreeMap::new();
    for (index, status) in additional.iter().enumerate() {
        let Some(root) = status.active.as_deref() else {
            continue;
        };
        let Ok(Some(checkout)) = crate::core::git_checkout::discover_checkout(root) else {
            continue;
        };
        by_repository
            .entry(checkout.repository.repository_key)
            .or_default()
            .push((index, checkout.checkout_key, checkout.checkout_generation));
    }

    for entries in by_repository.values() {
        let distinct: std::collections::BTreeSet<_> = entries
            .iter()
            .map(|(_, key, generation)| (key.clone(), generation.clone()))
            .collect();
        if distinct.len() <= 1 {
            continue;
        }
        for (index, _, _) in entries {
            additional[*index].active = None;
            additional[*index].state = WorkspaceRootState::SiblingWorktree;
        }
    }
}

fn resolve_with_bindings(
    value: StoredWorkspace,
    checkout_bindings: std::collections::BTreeMap<String, StoredCheckoutBinding>,
) -> ResolvedWorkspace {
    let primary = resolve_root(&value.primary, checkout_bindings.get(&value.primary));
    let mut additional: Vec<_> = value
        .additional
        .iter()
        .map(|root| resolve_root(root, checkout_bindings.get(root)))
        .collect();
    suppress_sibling_worktree_authority(&primary, &mut additional);
    let authority_fingerprint = authority_fingerprint(&value, &primary, &additional);
    ResolvedWorkspace {
        configured: value,
        checkout_bindings,
        primary,
        additional,
        authority_fingerprint,
    }
}

#[cfg(test)]
pub(crate) fn resolve(value: StoredWorkspace) -> ResolvedWorkspace {
    resolve_with_bindings(value, std::collections::BTreeMap::new())
}

pub(crate) fn load_resolved(path: &Path) -> Result<ResolvedWorkspace, String> {
    #[cfg(unix)]
    {
        let anchor = StoreAnchor::open(path)?;
        let lock = anchor.open_lock()?;
        FileExt::lock_shared(&lock).map_err(|e| e.to_string())?;
        let result = (|| {
            let value = read_unlocked(&anchor)?;
            let metadata = read_metadata_unlocked(&anchor, &value)?;
            let bindings = metadata
                .map(|metadata| metadata.checkout_bindings)
                .unwrap_or_default();
            Ok(resolve_with_bindings(value, bindings))
        })();
        let _ = FileExt::unlock(&lock);
        result
    }
    #[cfg(not(unix))]
    {
        let lock = private_options()
            .create(true)
            .open(path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        FileExt::lock_shared(&lock).map_err(|e| e.to_string())?;
        let result = (|| {
            let value = read_unlocked(path)?;
            let metadata = read_metadata_unlocked(path, &value)?;
            let bindings = metadata
                .map(|metadata| metadata.checkout_bindings)
                .unwrap_or_default();
            Ok(resolve_with_bindings(value, bindings))
        })();
        let _ = FileExt::unlock(&lock);
        result
    }
}

pub(crate) fn admin_view(path: &Path) -> Result<WorkspaceAdminView, String> {
    let resolved = load_resolved(path)?;
    Ok(WorkspaceAdminView {
        id: resolved.configured.id,
        revision: resolved.configured.revision,
        primary: resolved.configured.primary,
        additional: resolved.configured.additional,
        checkout_bindings: resolved.checkout_bindings,
        primary_status: resolved.primary,
        additional_statuses: resolved.additional,
    })
}

fn reject_store_in_roots(path: &Path, value: &StoredWorkspace) -> Result<(), String> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if canonical.starts_with(&value.primary)
        || value
            .additional
            .iter()
            .any(|root| canonical.starts_with(root))
    {
        return Err("Workspace state must be outside all workspace roots".into());
    }
    Ok(())
}

#[cfg(unix)]
fn atomic_save(anchor: &StoreAnchor, value: &StoredWorkspace) -> Result<(), String> {
    let temporary = anchor.temporary_name();
    let result = (|| -> Result<(), String> {
        let mut file = anchor.open_named(
            &temporary,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        verify_private_file(&file)?;
        file.write_all(&serde_json::to_vec(value).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        #[cfg(debug_assertions)]
        crate::server::workspace_test_hooks::external_hit("store_before_rename");
        anchor.rename_within(&temporary, &anchor.name)?;
        #[cfg(debug_assertions)]
        crate::server::workspace_test_hooks::external_hit("store_after_rename_before_dir_fsync");
        anchor.sync_dir()
    })();
    if result.is_err() {
        anchor.unlink(&temporary);
    }
    result
}

fn metadata_payload(records: &[StoredWorkspaceMetadata]) -> Result<Vec<u8>, String> {
    if records.is_empty() || records.len() > 2 {
        return Err("Workspace metadata journal must contain one or two revisions".into());
    }
    let journal = StoredWorkspaceMetadataJournal {
        schema_version: WORKSPACE_METADATA_SCHEMA_VERSION,
        versions: records.to_vec(),
    };
    serde_json::to_vec(&journal).map_err(|e| e.to_string())
}

#[cfg(unix)]
fn atomic_save_metadata_records(
    anchor: &StoreAnchor,
    records: &[StoredWorkspaceMetadata],
) -> Result<(), String> {
    let target = anchor.metadata_name();
    let temporary = anchor.temporary_name_for(&target);
    let payload = metadata_payload(records)?;
    let result = (|| -> Result<(), String> {
        let mut file = anchor.open_named(
            &temporary,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        verify_private_file(&file)?;
        file.write_all(&payload).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        anchor.rename_within(&temporary, &target)?;
        anchor.sync_dir()
    })();
    if result.is_err() {
        anchor.unlink(&temporary);
    }
    result
}

#[cfg(unix)]
fn atomic_save_metadata(
    anchor: &StoreAnchor,
    metadata: &StoredWorkspaceMetadata,
) -> Result<(), String> {
    atomic_save_metadata_records(anchor, std::slice::from_ref(metadata))
}

#[cfg(not(unix))]
fn atomic_save_metadata_records(
    path: &Path,
    records: &[StoredWorkspaceMetadata],
) -> Result<(), String> {
    let path = metadata_path(path);
    let name = path
        .file_name()
        .ok_or("Missing metadata filename")?
        .to_string_lossy()
        .into_owned();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let payload = metadata_payload(records)?;
    let result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
        file.write_all(&payload).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
        sync_parent(&path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(unix))]
fn atomic_save_metadata(path: &Path, metadata: &StoredWorkspaceMetadata) -> Result<(), String> {
    atomic_save_metadata_records(path, std::slice::from_ref(metadata))
}

#[cfg(windows)]
fn sync_parent(_path: &Path) -> Result<(), String> {
    // Windows directory handles cannot be opened/flushed through std::fs::File:
    // this always returned ERROR_ACCESS_DENIED after a successfully synced
    // Workspace file. File::sync_all() still protects the file contents.
    // Directory-entry durability is not claimed on Windows; the journal and
    // revision fence handle interrupted updates and reject mixed revisions.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("Missing parent directory")?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn atomic_save(path: &Path, value: &StoredWorkspace) -> Result<(), String> {
    let name = path
        .file_name()
        .ok_or("Missing state filename")?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        let mut file = private_options()
            .create_new(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec(value).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
pub(crate) fn load(path: &Path) -> Result<StoredWorkspace, String> {
    #[cfg(unix)]
    {
        let anchor = StoreAnchor::open(path)?;
        let lock = anchor.open_lock()?;
        FileExt::lock_shared(&lock).map_err(|e| e.to_string())?;
        let result = read_unlocked(&anchor);
        let _ = FileExt::unlock(&lock);
        result
    }
    #[cfg(not(unix))]
    {
        let lock = private_options()
            .create(true)
            .open(path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        FileExt::lock_shared(&lock).map_err(|e| e.to_string())?;
        let result = read_unlocked(path);
        let _ = FileExt::unlock(&lock);
        result
    }
}

fn reject_same_repository_worktree(
    value: &StoredWorkspace,
    bindings: &std::collections::BTreeMap<String, StoredCheckoutBinding>,
    candidate: &crate::core::git_checkout::CheckoutIdentity,
) -> Result<(), String> {
    for configured in std::iter::once(&value.primary).chain(value.additional.iter()) {
        if let Some(binding) = bindings.get(configured) {
            if binding.repository_key == candidate.repository.repository_key
                && !checkout_matches_binding(candidate, binding)
            {
                return Err(format!(
                    "Workspace root is a sibling worktree (another worktree generation/checkout of the same local Git repository) of configured root {configured}; worktree topology does not grant independent Workspace authority"
                ));
            }
            continue;
        }

        let existing = match crate::core::git_checkout::discover_checkout(Path::new(configured)) {
            Ok(existing) => existing,
            Err(crate::core::git_checkout::GitCheckoutError::ProbePathMissing(_)) => {
                // A legacy configured root may have disappeared before checkout identity
                // metadata existed. It is inactive authority, so it cannot prove a
                // same-repository conflict and must not block unrelated additions.
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect configured Git checkout {configured}: {error}"
                ));
            }
        };
        if let Some(existing) = existing
            && crate::core::git_checkout::same_repository(&existing, candidate)
            && existing.checkout_key != candidate.checkout_key
        {
            return Err(format!(
                "Workspace root is a sibling worktree of configured root {configured}; worktree topology does not grant independent Workspace authority"
            ));
        }
    }
    Ok(())
}

fn configured_additional_for_checkout(
    value: &StoredWorkspace,
    bindings: &std::collections::BTreeMap<String, StoredCheckoutBinding>,
    checkout: &crate::core::git_checkout::CheckoutIdentity,
) -> Option<String> {
    value.additional.iter().find_map(|configured| {
        bindings
            .get(configured)
            .is_some_and(|binding| checkout_matches_binding(checkout, binding))
            .then(|| configured.clone())
    })
}

fn configured_root_for_removal(
    value: &StoredWorkspace,
    bindings: &std::collections::BTreeMap<String, StoredCheckoutBinding>,
    requested: &str,
) -> String {
    if value.additional.iter().any(|root| root == requested) {
        return requested.to_string();
    }
    let Ok(Some(checkout)) = crate::core::git_checkout::discover_checkout(Path::new(requested))
    else {
        return requested.to_string();
    };
    value
        .additional
        .iter()
        .find(|configured| {
            bindings
                .get(*configured)
                .is_some_and(|binding| checkout_matches_binding(&checkout, binding))
        })
        .cloned()
        .unwrap_or_else(|| requested.to_string())
}

fn requested_is_primary(
    value: &StoredWorkspace,
    bindings: &std::collections::BTreeMap<String, StoredCheckoutBinding>,
    requested: &str,
) -> bool {
    if requested == value.primary {
        return true;
    }
    let Some(binding) = bindings.get(&value.primary) else {
        return false;
    };
    crate::core::git_checkout::discover_checkout(Path::new(requested))
        .ok()
        .flatten()
        .is_some_and(|checkout| checkout_matches_binding(&checkout, binding))
}

fn apply_mutation(
    value: &mut StoredWorkspace,
    bindings: &mut std::collections::BTreeMap<String, StoredCheckoutBinding>,
    root: &str,
    add: bool,
) -> Result<bool, String> {
    if requested_is_primary(value, bindings, root) {
        return Err("Primary root is immutable".into());
    }

    let before_roots = value.additional.clone();
    let before_bindings = bindings.clone();

    if add {
        if let Some(candidate) = crate::core::git_checkout::discover_checkout(Path::new(root))
            .map_err(|error| format!("cannot inspect candidate Git checkout: {error}"))?
        {
            if let Some(configured) =
                configured_additional_for_checkout(value, bindings, &candidate)
                && configured != root
            {
                value.additional.retain(|entry| entry != &configured);
                bindings.remove(&configured);
            }
            reject_same_repository_worktree(value, bindings, &candidate)?;
        }

        if !value.additional.iter().any(|entry| entry == root) {
            value.additional.push(root.to_string());
        }
        if let Some(binding) = binding_for_root(root) {
            bindings.insert(root.to_string(), binding);
        }
    } else {
        let configured = configured_root_for_removal(value, bindings, root);
        value.additional.retain(|entry| entry != &configured);
        bindings.remove(&configured);
    }

    value.additional.sort();
    Ok(value.additional != before_roots || *bindings != before_bindings)
}

fn add_missing_live_bindings(
    value: &StoredWorkspace,
    bindings: &mut std::collections::BTreeMap<String, StoredCheckoutBinding>,
) -> bool {
    let before = bindings.len();
    for root in std::iter::once(&value.primary).chain(value.additional.iter()) {
        if bindings.contains_key(root) {
            continue;
        }
        if let Some(binding) = binding_for_root(root) {
            bindings.insert(root.clone(), binding);
        }
    }
    bindings.len() != before
}

fn reconcile_bindings(
    value: &StoredWorkspace,
    existing: Option<StoredWorkspaceMetadata>,
) -> Result<
    (
        std::collections::BTreeMap<String, StoredCheckoutBinding>,
        bool,
    ),
    String,
> {
    let had_current_metadata = existing.as_ref().is_some_and(|metadata| {
        metadata.workspace_id == value.id && metadata.revision == value.revision
    });
    let mut bindings = match existing {
        Some(metadata) if metadata.workspace_id == value.id => metadata.checkout_bindings,
        Some(_) | None => std::collections::BTreeMap::new(),
    };

    let original_bindings = bindings.clone();
    bindings.retain(|root, _| root == &value.primary || value.additional.contains(root));
    add_missing_live_bindings(value, &mut bindings);

    let candidate = metadata_for(value, bindings.clone());
    validate_metadata(value, candidate)?;
    Ok((
        bindings.clone(),
        !had_current_metadata || bindings != original_bindings,
    ))
}

pub(crate) fn reconcile_metadata(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let anchor = StoreAnchor::open(path)?;
        let lock = anchor.open_lock()?;
        FileExt::lock_exclusive(&lock).map_err(|e| e.to_string())?;
        let value = read_unlocked(&anchor)?;
        let existing = read_metadata_raw_unlocked(&anchor)?.and_then(|records| {
            records.into_iter().find(|metadata| {
                metadata.workspace_id == value.id && metadata.revision == value.revision
            })
        });
        let (bindings, changed) = reconcile_bindings(&value, existing)?;
        if changed {
            let metadata = metadata_for(&value, bindings);
            atomic_save_metadata(&anchor, &metadata)?;
        }
        let _ = FileExt::unlock(&lock);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let lock = private_options()
            .create(true)
            .open(path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        FileExt::lock_exclusive(&lock).map_err(|e| e.to_string())?;
        let value = read_unlocked(path)?;
        let existing = read_metadata_raw_unlocked(path)?.and_then(|records| {
            records.into_iter().find(|metadata| {
                metadata.workspace_id == value.id && metadata.revision == value.revision
            })
        });
        let (bindings, changed) = reconcile_bindings(&value, existing)?;
        if changed {
            let metadata = metadata_for(&value, bindings);
            atomic_save_metadata(path, &metadata)?;
        }
        let _ = FileExt::unlock(&lock);
        Ok(())
    }
}

pub(crate) fn mutate(
    path: &Path,
    expected: u64,
    root: &str,
    add: bool,
) -> Result<StoredWorkspace, String> {
    let root = if add {
        validate_root(root)?
    } else if let Ok(root) = validate_root(root) {
        root
    } else {
        validate_configured_root(root)?;
        root.to_string()
    };
    #[cfg(unix)]
    {
        let anchor = StoreAnchor::open(path)?;
        let lock = anchor.open_lock()?;
        FileExt::lock_exclusive(&lock).map_err(|e| e.to_string())?;
        let mut value = read_unlocked(&anchor)?;
        if value.revision != expected {
            return Err("Workspace revision conflict".into());
        }
        let mut bindings = read_metadata_unlocked(&anchor, &value)?
            .map(|metadata| metadata.checkout_bindings)
            .unwrap_or_else(|| derive_live_bindings(&value));
        let previous_metadata = metadata_for(&value, bindings.clone());
        if apply_mutation(&mut value, &mut bindings, &root, add)? {
            reject_store_in_roots(&anchor.store_display_path(), &value)?;
            value.revision = value.revision.checked_add(1).ok_or("Revision overflow")?;
            let metadata = metadata_for(&value, bindings);
            // Publish a two-revision sidecar first. Before the legacy JSON
            // rename readers select the previous record; afterwards they select
            // the new record. A crash at either boundary therefore preserves an
            // exact Git checkout-identity fence instead of path-only semantics.
            atomic_save_metadata_records(&anchor, &[previous_metadata, metadata.clone()])?;
            atomic_save(&anchor, &value)?;
            // Compaction is optional for correctness: if it fails the bounded
            // two-revision journal remains valid.
            if let Err(error) = atomic_save_metadata(&anchor, &metadata) {
                tracing::warn!(%error, "could not compact Workspace metadata journal");
            }
        }
        let _ = FileExt::unlock(&lock);
        Ok(value)
    }
    #[cfg(not(unix))]
    {
        let lock = private_options()
            .create(true)
            .open(path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        FileExt::lock_exclusive(&lock).map_err(|e| e.to_string())?;
        let mut value = read_unlocked(path)?;
        if value.revision != expected {
            return Err("Workspace revision conflict".into());
        }
        let mut bindings = read_metadata_unlocked(path, &value)?
            .map(|metadata| metadata.checkout_bindings)
            .unwrap_or_else(|| derive_live_bindings(&value));
        let previous_metadata = metadata_for(&value, bindings.clone());
        if apply_mutation(&mut value, &mut bindings, &root, add)? {
            reject_store_in_roots(path, &value)?;
            value.revision = value.revision.checked_add(1).ok_or("Revision overflow")?;
            let metadata = metadata_for(&value, bindings);
            atomic_save_metadata_records(path, &[previous_metadata, metadata.clone()])?;
            atomic_save(path, &value)?;
            if let Err(error) = atomic_save_metadata(path, &metadata) {
                tracing::warn!(%error, "could not compact Workspace metadata journal");
            }
        }
        let _ = FileExt::unlock(&lock);
        Ok(value)
    }
}

pub(crate) fn initialized_store(path: &Path, primary: &str) -> Result<(), String> {
    let primary = validate_root(primary)?;
    let primary_binding = binding_for_root(&primary);
    #[cfg(unix)]
    {
        let anchor = StoreAnchor::open(path)?;
        let lock = anchor.open_lock()?;
        FileExt::lock_exclusive(&lock).map_err(|e| e.to_string())?;
        let value = StoredWorkspace {
            id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            primary,
            additional: vec![],
        };
        reject_store_in_roots(&anchor.store_display_path(), &value)?;
        let mut file = anchor.create_store().map_err(|e| {
            if e.contains("File exists") {
                "Workspace already exists".to_string()
            } else {
                e
            }
        })?;
        file.write_all(&serde_json::to_vec(&value).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        let bindings = primary_binding
            .clone()
            .into_iter()
            .map(|binding| (value.primary.clone(), binding))
            .collect();
        let metadata = metadata_for(&value, bindings);
        if let Err(error) = atomic_save_metadata(&anchor, &metadata) {
            anchor.unlink(&anchor.name);
            let _ = anchor.sync_dir();
            return Err(error);
        }
        anchor.sync_dir()?;
        let _ = FileExt::unlock(&lock);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        validate_store_path(path)?;
        if path.exists() {
            return Err("Workspace already exists".into());
        }
        let value = StoredWorkspace {
            id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            primary,
            additional: vec![],
        };
        reject_store_in_roots(path, &value)?;
        let mut file = private_options()
            .create_new(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec(&value).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        let bindings = primary_binding
            .into_iter()
            .map(|binding| (value.primary.clone(), binding))
            .collect();
        let metadata = metadata_for(&value, bindings);
        if let Err(error) = atomic_save_metadata(path, &metadata) {
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        sync_parent(path)?;
        Ok(())
    }
}

#[cfg(debug_assertions)]
mod debug_lifecycle;
#[cfg(debug_assertions)]
pub(crate) use debug_lifecycle::debug_worktree_lifecycle_self_test;

#[cfg(test)]
mod lifecycle_tests;
