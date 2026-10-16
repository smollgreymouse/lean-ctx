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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredWorkspace {
    pub id: String,
    pub revision: u64,
    pub primary: String,
    pub additional: Vec<String>,
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

    fn temporary_name(&self) -> std::ffi::OsString {
        let mut name = std::ffi::OsString::from(".");
        name.push(&self.name);
        name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
        name
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

fn validate_value(store_path: &Path, value: StoredWorkspace) -> Result<StoredWorkspace, String> {
    if value.revision == 0 || uuid::Uuid::parse_str(&value.id).is_err() {
        return Err("Invalid Workspace identity or revision".into());
    }
    if validate_root(&value.primary)? != value.primary {
        return Err("Noncanonical primary root".into());
    }
    for root in &value.additional {
        if validate_root(root)? != *root || root == &value.primary {
            return Err("Invalid or duplicate Workspace root".into());
        }
    }
    if value.additional.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("Workspace roots must be sorted and unique".into());
    }
    reject_store_in_roots(store_path, &value)?;
    Ok(value)
}

pub(crate) fn validate_root(root: &str) -> Result<String, String> {
    let canonical = Path::new(root).canonicalize().map_err(|e| e.to_string())?;
    if !canonical.is_dir() || crate::core::pathutil::is_broad_or_unsafe_root(&canonical) {
        return Err("Workspace root is unavailable or unsafe".into());
    }
    Ok(canonical.to_string_lossy().into_owned())
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

pub(crate) fn mutate(
    path: &Path,
    expected: u64,
    root: &str,
    add: bool,
) -> Result<StoredWorkspace, String> {
    let root = validate_root(root)?;
    #[cfg(unix)]
    {
        let anchor = StoreAnchor::open(path)?;
        let lock = anchor.open_lock()?;
        FileExt::lock_exclusive(&lock).map_err(|e| e.to_string())?;
        let mut value = read_unlocked(&anchor)?;
        if value.revision != expected {
            return Err("Workspace revision conflict".into());
        }
        if root == value.primary {
            return Err("Primary root is immutable".into());
        }
        let before = value.additional.clone();
        if add {
            if !value.additional.contains(&root) {
                value.additional.push(root);
            }
        } else {
            value.additional.retain(|entry| entry != &root);
        }
        value.additional.sort();
        if value.additional != before {
            reject_store_in_roots(&anchor.store_display_path(), &value)?;
            value.revision = value.revision.checked_add(1).ok_or("Revision overflow")?;
            atomic_save(&anchor, &value)?;
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
        if root == value.primary {
            return Err("Primary root is immutable".into());
        }
        let before = value.additional.clone();
        if add {
            if !value.additional.contains(&root) {
                value.additional.push(root);
            }
        } else {
            value.additional.retain(|entry| entry != &root);
        }
        value.additional.sort();
        if value.additional != before {
            reject_store_in_roots(path, &value)?;
            value.revision = value.revision.checked_add(1).ok_or("Revision overflow")?;
            atomic_save(path, &value)?;
        }
        let _ = FileExt::unlock(&lock);
        Ok(value)
    }
}

pub(crate) fn initialized_store(path: &Path, primary: &str) -> Result<(), String> {
    let primary = validate_root(primary)?;
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
        sync_parent(path)?;
        Ok(())
    }
}
