// SPDX-License-Identifier: Apache-2.0
//! Launcher-bound persisted Workspace. MCP requests cannot select a backing file.
//! A stable lock file serializes readers/writers across processes; writes replace
//! the JSON atomically and are never performed inside an allowed project root.
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
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

fn lock_path(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .ok_or("Workspace file name missing")?
        .to_string_lossy();
    Ok(path.with_file_name(format!("{file_name}.lock")))
}

fn acquire_lock_file(path: &Path) -> Result<std::fs::File, String> {
    validate_store_path(path)?;
    let lock = private_options()
        .create(true)
        .open(lock_path(path)?)
        .map_err(|e| e.to_string())?;
    verify_private_file(&lock)?;
    Ok(lock)
}

fn read_unlocked(path: &Path) -> Result<StoredWorkspace, String> {
    validate_store_path(path)?;
    let file = private_options().open(path).map_err(|e| e.to_string())?;
    verify_private_file(&file)?;
    let mut data = String::new();
    (&file)
        .read_to_string(&mut data)
        .map_err(|e| e.to_string())?;
    let value: StoredWorkspace = serde_json::from_str(&data).map_err(|e| e.to_string())?;
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
    reject_store_in_roots(path, &value)?;
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

fn sync_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("Missing parent directory")?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())
}

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
    let lock = acquire_lock_file(path)?;
    FileExt::lock_shared(&lock).map_err(|e| e.to_string())?;
    let result = read_unlocked(path);
    let _ = FileExt::unlock(&lock);
    result
}

pub(crate) fn mutate(
    path: &Path,
    expected: u64,
    root: &str,
    add: bool,
) -> Result<StoredWorkspace, String> {
    let root = validate_root(root)?;
    let lock = acquire_lock_file(path)?;
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

pub(crate) fn initialized_store(path: &Path, primary: &str) -> Result<(), String> {
    let primary = validate_root(primary)?;
    validate_store_path(path)?;
    let lock = acquire_lock_file(path)?;
    FileExt::lock_exclusive(&lock).map_err(|e| e.to_string())?;
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
    // Exclusive file creation prevents accidentally clobbering another owner.
    let mut file = private_options()
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec(&value).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    sync_parent(path)?;
    let _ = FileExt::unlock(&lock);
    Ok(())
}
