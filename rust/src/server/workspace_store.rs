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
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum WorkspaceRootState {
    Active,
    Moved,
    Missing,
    Unavailable,
    Unsafe,
    IdentityMismatch,
    SiblingWorktree,
}

#[derive(Debug, Clone)]
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
    let canonical = Path::new(root).canonicalize().map_err(|e| e.to_string())?;
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

#[cfg(not(unix))]
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
                    "Workspace root is another worktree generation/checkout of the same local Git                      repository as configured root {configured}; worktree topology does not grant                      independent Workspace authority"
                ));
            }
            continue;
        }

        let existing = crate::core::git_checkout::discover_checkout(Path::new(configured))
            .map_err(|error| {
                format!("cannot inspect configured Git checkout {configured}: {error}")
            })?;
        if let Some(existing) = existing
            && crate::core::git_checkout::same_repository(&existing, candidate)
            && existing.checkout_key != candidate.checkout_key
        {
            return Err(format!(
                "Workspace root is a sibling worktree of configured root {configured}; worktree                  topology does not grant independent Workspace authority"
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
    } else {
        match validate_root(root) {
            Ok(root) => root,
            Err(_) => {
                validate_configured_root(root)?;
                root.to_string()
            }
        }
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
pub(crate) fn debug_worktree_lifecycle_self_test() -> Result<(), String> {
    use std::fs;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "Workspace Self Test")
            .env("GIT_AUTHOR_EMAIL", "workspace-self-test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Workspace Self Test")
            .env("GIT_COMMITTER_EMAIL", "workspace-self-test@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    let tmp = tempfile::tempdir().map_err(|error| error.to_string())?;

    // 1. Missing additional is degraded state and remains explicitly removable.
    let plain_primary = tmp.path().join("plain-primary");
    let plain_extra = tmp.path().join("plain-extra");
    fs::create_dir(&plain_primary).map_err(|error| error.to_string())?;
    fs::create_dir(&plain_extra).map_err(|error| error.to_string())?;
    let plain_store = tmp.path().join("plain-workspace.json");
    initialized_store(&plain_store, plain_primary.to_str().ok_or("utf8 primary")?)?;
    mutate(
        &plain_store,
        1,
        plain_extra.to_str().ok_or("utf8 extra")?,
        true,
    )?;
    fs::remove_dir_all(&plain_extra).map_err(|error| error.to_string())?;
    let degraded = load_resolved(&plain_store)?;
    if degraded.additional.len() != 1
        || degraded.additional[0].state != WorkspaceRootState::Missing
        || !degraded.active_additional().is_empty()
    {
        return Err("missing additional root remained effective authority".into());
    }
    let removed = mutate(
        &plain_store,
        2,
        plain_extra.to_str().ok_or("utf8 extra")?,
        false,
    )?;
    if removed.revision != 3 || !removed.additional.is_empty() {
        return Err("missing additional root was not removable by CAS".into());
    }

    // 2. Linked-primary identity follows worktree move.
    let repo = tmp.path().join("repo");
    git(tmp.path(), &["init", repo.to_str().ok_or("utf8 repo")?])?;
    fs::write(repo.join("tracked.txt"), "main\n").map_err(|error| error.to_string())?;
    git(&repo, &["add", "tracked.txt"])?;
    git(&repo, &["commit", "-m", "initial"])?;
    git(&repo, &["branch", "feature-a"])?;
    git(&repo, &["branch", "feature-b"])?;

    let linked = tmp.path().join("linked");
    git(
        &repo,
        &[
            "worktree",
            "add",
            linked.to_str().ok_or("utf8 linked")?,
            "feature-a",
        ],
    )?;
    let git_store = tmp.path().join("git-workspace.json");
    initialized_store(&git_store, linked.to_str().ok_or("utf8 linked")?)?;
    let before = load_resolved(&git_store)?;
    let before_binding = before
        .checkout_bindings
        .get(linked.to_str().ok_or("utf8 linked")?)
        .cloned()
        .ok_or("missing persisted checkout identity")?;

    let moved = tmp.path().join("moved");
    git(
        &repo,
        &[
            "worktree",
            "move",
            linked.to_str().ok_or("utf8 linked")?,
            moved.to_str().ok_or("utf8 moved")?,
        ],
    )?;
    let after_move = load_resolved(&git_store)?;
    if after_move.primary.state != WorkspaceRootState::Moved
        || after_move.active_primary() != Some(moved.as_path())
        || before.authority_fingerprint == after_move.authority_fingerprint
    {
        return Err("worktree move did not preserve identity/change authority fingerprint".into());
    }

    // 3. Same repository sibling checkout cannot become additional authority.
    let sibling = tmp.path().join("sibling");
    git(
        &repo,
        &[
            "worktree",
            "add",
            sibling.to_str().ok_or("utf8 sibling")?,
            "feature-b",
        ],
    )?;
    let error = mutate(&git_store, 1, sibling.to_str().ok_or("utf8 sibling")?, true)
        .expect_err("sibling worktree must be rejected");
    if !error.contains("worktree") {
        return Err(format!("unexpected sibling rejection: {error}"));
    }

    // 4. remove + re-add of the same Git slot/path must be a new generation.
    git(
        &repo,
        &["worktree", "remove", moved.to_str().ok_or("utf8 moved")?],
    )?;
    git(
        &repo,
        &[
            "worktree",
            "add",
            linked.to_str().ok_or("utf8 linked")?,
            "feature-a",
        ],
    )?;
    let recreated = crate::core::git_checkout::discover_checkout(&linked)
        .map_err(|error| error.to_string())?
        .ok_or("recreated checkout not discovered")?;
    if recreated.checkout_key != before_binding.checkout_key
        || recreated.checkout_generation == before_binding.checkout_generation
    {
        return Err("worktree generation failed to distinguish remove/re-add".into());
    }
    reconcile_metadata(&git_store)?;
    let after_recreate = load_resolved(&git_store)?;
    if after_recreate.primary.state != WorkspaceRootState::IdentityMismatch
        || after_recreate.active_primary().is_some()
    {
        return Err("recreated worktree inherited old Workspace authority".into());
    }

    // 5. Main JSON must remain old-reader compatible; identity lives in sidecar.
    let raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&git_store).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if raw.get("checkout_bindings").is_some() || !metadata_path(&git_store).is_file() {
        return Err("checkout identity leaked into legacy Workspace JSON".into());
    }

    Ok(())
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git must be available");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct RepoFixture {
        tmp: tempfile::TempDir,
        main: std::path::PathBuf,
    }

    impl RepoFixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let main = tmp.path().join("main");
            git(tmp.path(), &["init", main.to_str().unwrap()]);
            git(
                &main,
                &["config", "user.email", "workspace@example.invalid"],
            );
            git(&main, &["config", "user.name", "Workspace Test"]);
            fs::write(main.join("tracked.txt"), "main\n").unwrap();
            git(&main, &["add", "tracked.txt"]);
            git(&main, &["commit", "-m", "initial"]);
            git(&main, &["branch", "feature-a"]);
            Self { tmp, main }
        }

        fn linked(&self, name: &str) -> std::path::PathBuf {
            let path = self.tmp.path().join(name);
            git(
                &self.main,
                &["worktree", "add", path.to_str().unwrap(), "feature-a"],
            );
            path
        }

        fn store(&self, name: &str) -> std::path::PathBuf {
            self.tmp.path().join(name)
        }
    }

    #[test]
    fn main_workspace_json_remains_legacy_compatible_and_identity_lives_in_sidecar() {
        let fixture = RepoFixture::new();
        let store = fixture.store("workspace.json");
        initialized_store(&store, fixture.main.to_str().unwrap()).unwrap();

        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&store).unwrap()).unwrap();
        assert!(raw.get("checkout_bindings").is_none());
        assert_eq!(
            raw.as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["additional", "id", "primary", "revision"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );

        let meta = metadata_path(&store);
        assert!(meta.is_file());
        let resolved = load_resolved(&store).unwrap();
        assert!(
            resolved
                .checkout_bindings
                .contains_key(fixture.main.to_str().unwrap())
        );
    }

    #[test]
    fn stale_sidecar_from_legacy_writer_fails_closed_until_reconciled() {
        let fixture = RepoFixture::new();
        let store = fixture.store("workspace.json");
        initialized_store(&store, fixture.main.to_str().unwrap()).unwrap();
        let before = load_resolved(&store).unwrap();
        let before_binding = before
            .checkout_bindings
            .get(fixture.main.to_str().unwrap())
            .unwrap()
            .clone();

        // Simulate an old binary that knows only the legacy JSON: it bumps the
        // Workspace revision while leaving the sidecar untouched.
        let mut legacy = load(&store).unwrap();
        legacy.revision = 2;
        fs::write(&store, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let error = load_resolved(&store).expect_err("metadata fence mismatch must fail closed");
        assert!(error.contains("metadata/store fence mismatch"), "{error}");

        reconcile_metadata(&store).unwrap();
        let after = load_resolved(&store).unwrap();
        assert_eq!(after.configured.revision, 2);
        assert_eq!(
            after.checkout_bindings.get(fixture.main.to_str().unwrap()),
            Some(&before_binding),
            "reconcile must preserve established checkout identity"
        );
    }

    #[test]
    fn legacy_store_without_sidecar_can_be_identity_migrated_in_place() {
        let fixture = RepoFixture::new();
        let store = fixture.store("workspace.json");
        initialized_store(&store, fixture.main.to_str().unwrap()).unwrap();
        fs::remove_file(metadata_path(&store)).unwrap();

        let legacy = load_resolved(&store).unwrap();
        assert!(legacy.checkout_bindings.is_empty());
        assert_eq!(legacy.primary.state, WorkspaceRootState::Active);

        reconcile_metadata(&store).unwrap();
        let migrated = load_resolved(&store).unwrap();
        assert!(
            migrated
                .checkout_bindings
                .contains_key(fixture.main.to_str().unwrap())
        );
        assert_eq!(migrated.configured.revision, 1);
    }

    #[test]
    fn missing_additional_root_is_degraded_not_store_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("primary");
        let additional = tmp.path().join("additional");
        fs::create_dir(&primary).unwrap();
        fs::create_dir(&additional).unwrap();
        let store = tmp.path().join("workspace.json");

        initialized_store(&store, primary.to_str().unwrap()).unwrap();
        let added = mutate(&store, 1, additional.to_str().unwrap(), true).unwrap();
        assert_eq!(added.revision, 2);
        fs::remove_dir_all(&additional).unwrap();

        let configured = load(&store).expect("persisted configuration must remain readable");
        assert_eq!(configured.additional, vec![additional.to_string_lossy()]);

        let resolved = load_resolved(&store).unwrap();
        assert_eq!(resolved.primary.state, WorkspaceRootState::Active);
        assert_eq!(resolved.additional.len(), 1);
        assert_eq!(resolved.additional[0].state, WorkspaceRootState::Missing);
        assert!(resolved.active_additional().is_empty());
    }

    #[test]
    fn missing_additional_root_can_be_removed_after_external_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("primary");
        let additional = tmp.path().join("additional");
        fs::create_dir(&primary).unwrap();
        fs::create_dir(&additional).unwrap();
        let store = tmp.path().join("workspace.json");

        initialized_store(&store, primary.to_str().unwrap()).unwrap();
        mutate(&store, 1, additional.to_str().unwrap(), true).unwrap();
        fs::remove_dir_all(&additional).unwrap();

        let removed = mutate(&store, 2, additional.to_str().unwrap(), false).unwrap();
        assert_eq!(removed.revision, 3);
        assert!(removed.additional.is_empty());
        let resolved = load_resolved(&store).unwrap();
        assert!(
            !resolved
                .checkout_bindings
                .contains_key(additional.to_str().unwrap())
        );
    }

    #[test]
    fn legacy_store_does_not_grant_authority_to_primary_sibling_worktree() {
        let fixture = RepoFixture::new();
        let linked = fixture.linked("legacy-linked");
        let value = StoredWorkspace {
            id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            primary: fixture
                .main
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            additional: vec![
                linked
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ],
        };

        let resolved = resolve(value);
        assert_eq!(resolved.primary.state, WorkspaceRootState::Active);
        assert_eq!(
            resolved.additional[0].state,
            WorkspaceRootState::SiblingWorktree
        );
        assert!(resolved.active_additional().is_empty());
    }

    #[test]
    fn legacy_two_additional_sibling_worktrees_are_both_fail_closed() {
        let primary_tmp = tempfile::tempdir().unwrap();
        let primary = primary_tmp.path().join("primary");
        fs::create_dir(&primary).unwrap();

        let fixture = RepoFixture::new();
        git(&fixture.main, &["branch", "feature-b"]);
        let first = fixture.linked("legacy-first");
        let second = fixture.tmp.path().join("legacy-second");
        git(
            &fixture.main,
            &["worktree", "add", second.to_str().unwrap(), "feature-b"],
        );
        let mut additional = vec![
            first.canonicalize().unwrap().to_string_lossy().into_owned(),
            second
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ];
        additional.sort();
        let value = StoredWorkspace {
            id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            primary: primary
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            additional,
        };

        let resolved = resolve(value);
        assert!(
            resolved
                .additional
                .iter()
                .all(|status| status.state == WorkspaceRootState::SiblingWorktree)
        );
        assert!(resolved.active_additional().is_empty());
    }

    #[test]
    fn sibling_worktree_of_primary_repository_is_not_additional_authority() {
        let fixture = RepoFixture::new();
        let linked = fixture.linked("linked");
        let store = fixture.store("workspace.json");
        initialized_store(&store, fixture.main.to_str().unwrap()).unwrap();

        let error = mutate(&store, 1, linked.to_str().unwrap(), true)
            .expect_err("sibling worktree must be topology, not multi-root authority");
        assert!(error.contains("sibling worktree"), "{error}");
        assert!(load(&store).unwrap().additional.is_empty());
    }

    #[test]
    fn sibling_worktrees_cannot_both_be_additional_roots() {
        let primary_tmp = tempfile::tempdir().unwrap();
        let primary = primary_tmp.path().join("primary");
        fs::create_dir(&primary).unwrap();

        let fixture = RepoFixture::new();
        git(&fixture.main, &["branch", "feature-b"]);
        let first = fixture.linked("first");
        let second = fixture.tmp.path().join("second");
        git(
            &fixture.main,
            &["worktree", "add", second.to_str().unwrap(), "feature-b"],
        );

        let store = primary_tmp.path().join("workspace.json");
        initialized_store(&store, primary.to_str().unwrap()).unwrap();
        let first_state = mutate(&store, 1, first.to_str().unwrap(), true).unwrap();
        assert_eq!(first_state.revision, 2);

        let error = mutate(&store, 2, second.to_str().unwrap(), true)
            .expect_err("two checkout slots of one local repository must not both be authority");
        assert!(error.contains("same local Git repository"), "{error}");
        let current = load(&store).unwrap();
        assert_eq!(current.additional, vec![first.to_string_lossy()]);
        assert_eq!(current.revision, 2);
    }

    #[test]
    fn adding_current_path_of_moved_additional_rebinds_instead_of_duplicating() {
        let primary_tmp = tempfile::tempdir().unwrap();
        let primary = primary_tmp.path().join("primary");
        fs::create_dir(&primary).unwrap();

        let fixture = RepoFixture::new();
        let linked = fixture.linked("linked-rebind");
        let moved = fixture.tmp.path().join("moved-rebind");
        let store = primary_tmp.path().join("workspace-rebind.json");
        initialized_store(&store, primary.to_str().unwrap()).unwrap();
        mutate(&store, 1, linked.to_str().unwrap(), true).unwrap();

        git(
            &fixture.main,
            &[
                "worktree",
                "move",
                linked.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );

        let rebound = mutate(&store, 2, moved.to_str().unwrap(), true).unwrap();
        assert_eq!(rebound.revision, 3);
        assert_eq!(rebound.additional, vec![moved.to_string_lossy()]);
        let rebound_resolved = load_resolved(&store).unwrap();
        assert!(
            !rebound_resolved
                .checkout_bindings
                .contains_key(linked.to_str().unwrap())
        );
        assert!(
            rebound_resolved
                .checkout_bindings
                .contains_key(moved.to_str().unwrap())
        );
    }

    #[test]
    fn linked_primary_follows_git_worktree_move_by_checkout_identity() {
        let fixture = RepoFixture::new();
        let linked = fixture.linked("linked");
        let moved = fixture.tmp.path().join("moved");
        let store = fixture.store("workspace.json");
        initialized_store(&store, linked.to_str().unwrap()).unwrap();

        let before = load_resolved(&store).unwrap();
        assert_eq!(before.primary.state, WorkspaceRootState::Active);
        assert!(
            before
                .checkout_bindings
                .contains_key(linked.to_str().unwrap())
        );

        git(
            &fixture.main,
            &[
                "worktree",
                "move",
                linked.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );

        let after = load_resolved(&store).unwrap();
        assert_eq!(after.configured.primary, linked.to_string_lossy());
        assert_eq!(after.primary.state, WorkspaceRootState::Moved);
        assert_eq!(after.active_primary(), Some(moved.as_path()));
        assert_ne!(before.authority_fingerprint, after.authority_fingerprint);
    }

    #[test]
    fn removed_linked_primary_is_missing_but_store_stays_readable() {
        let fixture = RepoFixture::new();
        let linked = fixture.linked("linked");
        let store = fixture.store("workspace.json");
        initialized_store(&store, linked.to_str().unwrap()).unwrap();

        git(
            &fixture.main,
            &["worktree", "remove", linked.to_str().unwrap()],
        );

        let configured = load(&store).expect("deleted primary worktree must not corrupt store");
        assert_eq!(configured.primary, linked.to_string_lossy());
        let resolved = load_resolved(&store).unwrap();
        assert_eq!(resolved.primary.state, WorkspaceRootState::Missing);
        assert!(resolved.active_primary().is_none());
    }

    #[test]
    fn same_repository_recreated_worktree_slot_cannot_resurrect_old_authority() {
        let fixture = RepoFixture::new();
        let linked = fixture.linked("linked-generation");
        let store = fixture.store("workspace-generation.json");
        initialized_store(&store, linked.to_str().unwrap()).unwrap();

        let before = load_resolved(&store).unwrap();
        let before_binding = before
            .checkout_bindings
            .get(linked.to_str().unwrap())
            .expect("Git primary must persist checkout binding")
            .clone();

        git(
            &fixture.main,
            &["worktree", "remove", linked.to_str().unwrap()],
        );
        git(
            &fixture.main,
            &["worktree", "add", linked.to_str().unwrap(), "feature-a"],
        );

        let recreated = crate::core::git_checkout::discover_checkout(&linked)
            .unwrap()
            .unwrap();
        assert_eq!(recreated.checkout_key, before_binding.checkout_key);
        assert_ne!(
            recreated.checkout_generation, before_binding.checkout_generation,
            "new worktree generation must differ even when Git reuses the admin path"
        );

        reconcile_metadata(&store).unwrap();
        let resolved = load_resolved(&store).unwrap();
        assert_eq!(resolved.primary.state, WorkspaceRootState::IdentityMismatch);
        assert!(resolved.active_primary().is_none());
    }

    #[test]
    fn foreign_checkout_reusing_old_worktree_path_cannot_resurrect_authority() {
        let fixture = RepoFixture::new();
        let linked = fixture.linked("linked");
        let store = fixture.store("workspace.json");
        initialized_store(&store, linked.to_str().unwrap()).unwrap();

        git(
            &fixture.main,
            &["worktree", "remove", linked.to_str().unwrap()],
        );
        git(fixture.tmp.path(), &["init", linked.to_str().unwrap()]);

        let resolved = load_resolved(&store).unwrap();
        assert_eq!(resolved.primary.state, WorkspaceRootState::IdentityMismatch);
        assert!(resolved.active_primary().is_none());
    }

    #[test]
    fn removing_moved_additional_by_current_path_uses_checkout_identity() {
        let primary_tmp = tempfile::tempdir().unwrap();
        let primary = primary_tmp.path().join("primary");
        fs::create_dir(&primary).unwrap();

        let fixture = RepoFixture::new();
        let linked = fixture.linked("linked");
        let moved = fixture.tmp.path().join("moved");
        let store = primary_tmp.path().join("workspace.json");
        initialized_store(&store, primary.to_str().unwrap()).unwrap();
        let added = mutate(&store, 1, linked.to_str().unwrap(), true).unwrap();
        assert_eq!(added.revision, 2);

        git(
            &fixture.main,
            &[
                "worktree",
                "move",
                linked.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );

        let resolved = load_resolved(&store).unwrap();
        assert_eq!(resolved.additional[0].state, WorkspaceRootState::Moved);
        assert_eq!(resolved.active_additional(), vec![moved.clone()]);

        let removed = mutate(&store, 2, moved.to_str().unwrap(), false).unwrap();
        assert_eq!(removed.revision, 3);
        assert!(removed.additional.is_empty());
        let removed_resolved = load_resolved(&store).unwrap();
        assert!(
            removed_resolved
                .checkout_bindings
                .get(linked.to_str().unwrap())
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn two_revision_metadata_journal_survives_store_rename_boundary() {
        let fixture = RepoFixture::new();
        let linked = fixture.linked("linked-journal");
        let store = fixture.store("workspace-journal.json");
        initialized_store(&store, linked.to_str().unwrap()).unwrap();

        let before = load_resolved(&store).unwrap();
        let old_value = before.configured.clone();
        let old_metadata = metadata_for(&old_value, before.checkout_bindings.clone());

        let mut new_value = old_value.clone();
        new_value.revision += 1;
        let new_metadata = metadata_for(&new_value, before.checkout_bindings.clone());

        let anchor = StoreAnchor::open(&store).unwrap();
        let lock = anchor.open_lock().unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        atomic_save_metadata_records(&anchor, &[old_metadata.clone(), new_metadata.clone()])
            .unwrap();
        FileExt::unlock(&lock).unwrap();

        let still_old = load_resolved(&store).unwrap();
        assert_eq!(still_old.configured.revision, old_value.revision);
        assert_eq!(still_old.primary.state, WorkspaceRootState::Active);
        assert_eq!(
            still_old.checkout_bindings.get(linked.to_str().unwrap()),
            old_metadata.checkout_bindings.get(linked.to_str().unwrap())
        );

        let anchor = StoreAnchor::open(&store).unwrap();
        let lock = anchor.open_lock().unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        atomic_save(&anchor, &new_value).unwrap();
        FileExt::unlock(&lock).unwrap();

        let after_rename = load_resolved(&store).unwrap();
        assert_eq!(after_rename.configured.revision, new_value.revision);
        assert_eq!(after_rename.primary.state, WorkspaceRootState::Active);
        assert_eq!(
            after_rename.checkout_bindings.get(linked.to_str().unwrap()),
            new_metadata.checkout_bindings.get(linked.to_str().unwrap())
        );
    }
}
