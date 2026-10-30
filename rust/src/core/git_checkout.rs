// SPDX-License-Identifier: Apache-2.0

//! Canonical Git checkout/worktree discovery.
//!
//! This module is deliberately about identity and topology only. Discovering a
//! sibling worktree never grants path/search authority.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepositoryIdentity {
    pub(crate) git_common_dir: PathBuf,
    pub(crate) portable_identity: Option<String>,
    pub(crate) repository_key: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckoutKind {
    Main,
    Linked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CheckoutIdentity {
    pub(crate) repository: RepositoryIdentity,
    /// Current canonical filesystem binding for this checkout.
    pub(crate) root: PathBuf,
    /// Canonical checkout-specific Git administration directory.
    pub(crate) git_dir: PathBuf,
    /// Stable local checkout-slot key. The root is intentionally not part of it:
    /// \`git worktree move\` keeps the admin slot while changing the root path.
    pub(crate) checkout_key: String,
    /// Filesystem generation of the Git admin slot. Unlike `checkout_key`,
    /// this changes when Git removes and later recreates a slot at the same
    /// admin path, preventing authority resurrection after remove/re-add.
    pub(crate) checkout_generation: Option<String>,
    pub(crate) kind: CheckoutKind,
    pub(crate) branch: Option<String>,
    pub(crate) head_oid: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiscoveredWorktree {
    pub(crate) root: PathBuf,
    pub(crate) git_dir: Option<PathBuf>,
    pub(crate) branch: Option<String>,
    pub(crate) head_oid: Option<String>,
    pub(crate) locked: bool,
    pub(crate) prunable: bool,
    pub(crate) exists: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GitCheckoutError {
    GitUnavailable(String),
    CommandFailed(String),
    InvalidMetadata(String),
    UnsafeResolvedPath(PathBuf),
}

impl fmt::Display for GitCheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitUnavailable(message) => write!(f, "git unavailable: {message}"),
            Self::CommandFailed(message) => write!(f, "git command failed: {message}"),
            Self::InvalidMetadata(message) => write!(f, "invalid git metadata: {message}"),
            Self::UnsafeResolvedPath(path) => {
                write!(f, "git resolved an unsafe project path: {}", path.display())
            }
        }
    }
}

impl std::error::Error for GitCheckoutError {}

/// Cheap filesystem-only checkout boundary used by hot path resolution.
/// A linked worktree has a \`.git\` file; a normal checkout has a \`.git\` directory.
pub(crate) fn nearest_checkout_root(start: &Path) -> Option<PathBuf> {
    let start = crate::core::pathutil::safe_canonicalize_or_self(start);
    let mut current: Option<&Path> = Some(start.as_path());
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn sanitize_git_environment(command: &mut Command) {
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("LANG", "C");

    // Checkout discovery must describe cwd, not an inherited Git override from
    // a launcher, hook, test process, or command-scoped Git config injection.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_SYSTEM",
    ] {
        command.env_remove(key);
    }

    if let Ok(count) = std::env::var("GIT_CONFIG_COUNT")
        && let Ok(count) = count.parse::<usize>()
    {
        for index in 0..count {
            command.env_remove(format!("GIT_CONFIG_KEY_{index}"));
            command.env_remove(format!("GIT_CONFIG_VALUE_{index}"));
        }
    }
}

fn command_output(cwd: &Path, args: &[&str]) -> Result<std::process::Output, GitCheckoutError> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    sanitize_git_environment(&mut command);

    command
        .output()
        .map_err(|error| GitCheckoutError::GitUnavailable(error.to_string()))
}

fn command_cwd(path: &Path) -> Result<PathBuf, GitCheckoutError> {
    let path = if path.is_dir() {
        path
    } else if path.is_file() {
        path.parent().ok_or_else(|| {
            GitCheckoutError::InvalidMetadata(format!("path has no parent: {}", path.display()))
        })?
    } else {
        return Err(GitCheckoutError::InvalidMetadata(format!(
            "checkout probe path does not exist: {}",
            path.display()
        )));
    };
    Ok(crate::core::pathutil::safe_canonicalize_or_self(path))
}

fn trim_git_path_bytes(mut bytes: &[u8]) -> &[u8] {
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf, GitCheckoutError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf, GitCheckoutError> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|_| GitCheckoutError::InvalidMetadata("Git path output was not UTF-8".into()))
}

#[cfg(not(any(unix, windows)))]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf, GitCheckoutError> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|_| GitCheckoutError::InvalidMetadata("Git path output was not UTF-8".into()))
}

fn path_output(cwd: &Path, args: &[&str]) -> Result<PathBuf, GitCheckoutError> {
    let output = command_output(cwd, args)?;
    if !output.status.success() {
        return Err(GitCheckoutError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let bytes = trim_git_path_bytes(&output.stdout);
    if bytes.is_empty() {
        return Err(GitCheckoutError::InvalidMetadata(format!(
            "empty output from git {}",
            args.join(" ")
        )));
    }
    path_from_git_bytes(bytes)
}

fn optional_text_output(cwd: &Path, args: &[&str]) -> Result<Option<String>, GitCheckoutError> {
    let output = command_output(cwd, args)?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

fn absolutize_path(cwd: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn canonical_existing(path: &Path, label: &str) -> Result<PathBuf, GitCheckoutError> {
    crate::core::pathutil::safe_canonicalize(path).map_err(|error| {
        GitCheckoutError::InvalidMetadata(format!(
            "cannot canonicalize {label} {}: {error}",
            path.display()
        ))
    })
}

fn hash_path_bytes(hasher: &mut blake3::Hasher, path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in path.as_os_str().encode_wide() {
            hasher.update(&unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        hasher.update(path.to_string_lossy().as_bytes());
    }
}

fn opaque_key(parts: &[&Path]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hash_path_bytes(&mut hasher, part);
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

/// Best-effort filesystem generation for a checkout admin slot.
///
/// The admin path itself is intentionally stable across `git worktree move`,
/// but Git may reuse that path after a remove/re-add. File identity distinguishes
/// those generations without writing marker files into Git's private metadata.
fn checkout_generation(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Some(format!("unix:{}:{}", metadata.dev(), metadata.ino()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return Some(format!(
            "windows:{}:{}",
            metadata.volume_serial_number()?,
            metadata.file_index()?
        ));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        None
    }
}

pub(crate) fn normalize_remote_url(url: &str) -> String {
    let url = url.trim().trim_end_matches(".git");
    let url = url
        .strip_prefix("git@")
        .map_or_else(|| url.to_string(), |value| value.replacen(':', "/", 1));
    url.to_lowercase()
}

pub(crate) fn origin_remote_url_from_config(config: &Path) -> Option<String> {
    let content = std::fs::read_to_string(config).ok()?;
    let mut in_origin = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_origin = trimmed == r#"[remote "origin"]"#;
            continue;
        }
        if in_origin && let Some(url) = trimmed.strip_prefix("url") {
            let url = url.trim_start_matches([' ', '=']).trim();
            if !url.is_empty() {
                return Some(normalize_remote_url(url));
            }
        }
    }
    None
}

/// Discover the concrete Git checkout containing \`path\`.
///
/// \`Ok(None)\` means "not a Git repository". Operational/discovery failures are
/// errors so future bridge/topology callers can fail closed.
pub(crate) fn discover_checkout(path: &Path) -> Result<Option<CheckoutIdentity>, GitCheckoutError> {
    let cwd = command_cwd(path)?;
    let top = command_output(&cwd, &["rev-parse", "--show-toplevel"])?;
    if !top.status.success() {
        let stderr = String::from_utf8_lossy(&top.stderr);
        if stderr.contains("not a git repository") || stderr.contains("not a git work tree") {
            return Ok(None);
        }
        return Err(GitCheckoutError::CommandFailed(stderr.trim().to_string()));
    }

    let root_path = path_from_git_bytes(trim_git_path_bytes(&top.stdout))?;
    let root = canonical_existing(&root_path, "worktree root")?;
    if crate::core::pathutil::is_broad_or_unsafe_root(&root) {
        return Err(GitCheckoutError::UnsafeResolvedPath(root));
    }

    let git_dir_raw = match path_output(&root, &["rev-parse", "--absolute-git-dir"]) {
        Ok(value) => value,
        Err(GitCheckoutError::CommandFailed(_)) => path_output(&root, &["rev-parse", "--git-dir"])?,
        Err(error) => return Err(error),
    };
    let git_dir_path = absolutize_path(&root, git_dir_raw);
    let git_dir = canonical_existing(&git_dir_path, "git dir")?;

    let common_raw = path_output(&root, &["rev-parse", "--git-common-dir"])?;
    let git_common_dir_path = absolutize_path(&root, common_raw);
    let git_common_dir = canonical_existing(&git_common_dir_path, "git common dir")?;

    let repository_key = opaque_key(&[git_common_dir.as_path()]);
    let portable_identity = origin_remote_url_from_config(&git_common_dir.join("config"))
        .map(|url| format!("git:{url}"));
    let checkout_key = opaque_key(&[git_common_dir.as_path(), git_dir.as_path()]);
    let kind = if git_dir == git_common_dir {
        CheckoutKind::Main
    } else {
        CheckoutKind::Linked
    };
    let branch = optional_text_output(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let head_oid = optional_text_output(&root, &["rev-parse", "--verify", "HEAD"])?;
    let checkout_generation = checkout_generation(&git_dir);

    Ok(Some(CheckoutIdentity {
        repository: RepositoryIdentity {
            git_common_dir,
            portable_identity,
            repository_key,
        },
        root,
        git_dir,
        checkout_key,
        checkout_generation,
        kind,
        branch,
        head_oid,
    }))
}

pub(crate) fn same_repository(a: &CheckoutIdentity, b: &CheckoutIdentity) -> bool {
    a.repository.repository_key == b.repository.repository_key
        && a.repository.git_common_dir == b.repository.git_common_dir
}

/// Resolve a previously known checkout from its stable Git administration
/// directory. Linked worktrees keep this admin slot across `git worktree move`,
/// so this is the bridge from persisted checkout identity to the current root.
///
/// The returned checkout is still fully rediscovered and validated; the stored
/// admin path is only a locator and never grants filesystem authority by itself.
pub(crate) fn resolve_checkout_by_git_dir(
    git_dir: &Path,
) -> Result<Option<CheckoutIdentity>, GitCheckoutError> {
    if !git_dir.exists() {
        return Ok(None);
    }
    let git_dir = canonical_existing(git_dir, "stored git dir")?;

    let candidate_root = if git_dir.join("gitdir").is_file() {
        let backlink = std::fs::read(git_dir.join("gitdir")).map_err(|error| {
            GitCheckoutError::InvalidMetadata(format!(
                "cannot read worktree gitdir backlink {}: {error}",
                git_dir.join("gitdir").display()
            ))
        })?;
        let backlink = path_from_git_bytes(trim_git_path_bytes(&backlink))?;
        let backlink = absolutize_path(&git_dir, backlink);
        let Some(root) = backlink.parent() else {
            return Err(GitCheckoutError::InvalidMetadata(
                "worktree gitdir backlink has no parent".into(),
            ));
        };
        root.to_path_buf()
    } else {
        let Some(root) = git_dir.parent() else {
            return Err(GitCheckoutError::InvalidMetadata(
                "main git dir has no repository parent".into(),
            ));
        };
        root.to_path_buf()
    };

    if !candidate_root.exists() {
        return Ok(None);
    }
    let Some(identity) = discover_checkout(&candidate_root)? else {
        return Ok(None);
    };
    if identity.git_dir != git_dir {
        return Err(GitCheckoutError::InvalidMetadata(format!(
            "stored git dir {} resolved to a different checkout admin dir {}",
            git_dir.display(),
            identity.git_dir.display()
        )));
    }
    Ok(Some(identity))
}

fn admin_dir_for_registered_root(common: &Path, root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(common.join("worktrees")).ok()?;
    let root = crate::core::pathutil::safe_canonicalize_or_self(root);
    for entry in entries.flatten() {
        let admin = entry.path();
        if !admin.is_dir() {
            continue;
        }
        let Ok(backlink) = std::fs::read(admin.join("gitdir")) else {
            continue;
        };
        let Ok(backlink) = path_from_git_bytes(trim_git_path_bytes(&backlink)) else {
            continue;
        };
        let backlink = absolutize_path(&admin, backlink);
        let Some(parent) = backlink.parent() else {
            continue;
        };
        if crate::core::pathutil::safe_canonicalize_or_self(parent) == root {
            return Some(crate::core::pathutil::safe_canonicalize_or_self(&admin));
        }
    }
    None
}

fn parse_worktree_record(
    record: &[&[u8]],
    common: &Path,
) -> Result<DiscoveredWorktree, GitCheckoutError> {
    let mut root: Option<PathBuf> = None;
    let mut branch = None;
    let mut head_oid = None;
    let mut locked = false;
    let mut prunable = false;

    for raw in record {
        if let Some(value) = raw.strip_prefix(b"worktree ") {
            root = Some(path_from_git_bytes(value)?);
            continue;
        }

        let field = String::from_utf8_lossy(raw);
        if let Some(value) = field.strip_prefix("HEAD ") {
            head_oid = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("branch ") {
            branch = Some(
                value
                    .strip_prefix("refs/heads/")
                    .unwrap_or(value)
                    .to_string(),
            );
        } else if field == "locked" || field.starts_with("locked ") {
            locked = true;
        } else if field == "prunable" || field.starts_with("prunable ") {
            prunable = true;
        }
    }

    let root = root
        .ok_or_else(|| GitCheckoutError::InvalidMetadata("worktree record has no root".into()))?;
    let exists = root.is_dir();
    let canonical_root = exists.then(|| crate::core::pathutil::safe_canonicalize_or_self(&root));
    let git_dir = if let Some(ref canonical_root) = canonical_root {
        match discover_checkout(canonical_root).ok().flatten() {
            Some(identity)
                if identity.root == *canonical_root
                    && identity.repository.git_common_dir == common =>
            {
                Some(identity.git_dir)
            }
            // The registered path now contains another live checkout. Never
            // reinterpret that foreign checkout as this repository's stale slot.
            Some(identity) if identity.root == *canonical_root => None,
            // An existing plain/recreated directory may sit inside another
            // checkout. Use the worktree-admin backlink to identify the stale
            // registration instead of the enclosing checkout.
            _ => admin_dir_for_registered_root(common, canonical_root),
        }
    } else {
        admin_dir_for_registered_root(common, &root)
    };

    Ok(DiscoveredWorktree {
        root: canonical_root.unwrap_or(root),
        git_dir,
        branch,
        head_oid,
        locked,
        prunable,
        exists,
    })
}

fn parse_porcelain_z(
    bytes: &[u8],
    common: &Path,
) -> Result<Vec<DiscoveredWorktree>, GitCheckoutError> {
    let mut out = Vec::new();
    let mut record = Vec::new();

    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if !record.is_empty() {
                out.push(parse_worktree_record(&record, common)?);
                record.clear();
            }
        } else {
            record.push(field);
        }
    }
    if !record.is_empty() {
        out.push(parse_worktree_record(&record, common)?);
    }
    Ok(out)
}

/// Enumerate sibling worktrees as metadata only.
///
/// The active checkout is discovered independently and wins over stale records
/// for the same checkout-specific \`git_dir\`.
pub(crate) fn list_worktrees(
    active: &CheckoutIdentity,
) -> Result<Vec<DiscoveredWorktree>, GitCheckoutError> {
    let output = command_output(&active.root, &["worktree", "list", "--porcelain", "-z"])?;
    if !output.status.success() {
        return Err(GitCheckoutError::CommandFailed(format!(
            "git worktree list --porcelain -z failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let mut listed = parse_porcelain_z(&output.stdout, &active.repository.git_common_dir)?;
    listed.retain(|entry| entry.git_dir.as_ref() != Some(&active.git_dir));
    // Existing roots were already verified during parsing. Drop a path that is
    // currently occupied by a foreign checkout; stale/missing registrations may
    // remain as metadata but are never authority.
    listed.retain(|entry| !entry.exists || entry.git_dir.is_some());

    listed.insert(
        0,
        DiscoveredWorktree {
            root: active.root.clone(),
            git_dir: Some(active.git_dir.clone()),
            branch: active.branch.clone(),
            head_oid: active.head_oid.clone(),
            locked: false,
            prunable: false,
            exists: true,
        },
    );
    Ok(listed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct RepoFixture {
        tmp: tempfile::TempDir,
        main: PathBuf,
    }

    impl RepoFixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let main = tmp.path().join("main");
            git(tmp.path(), &["init", main.to_str().unwrap()]);
            git(&main, &["config", "user.email", "test@example.invalid"]);
            git(&main, &["config", "user.name", "LeanCTX Test"]);
            fs::write(main.join("file.txt"), "main\n").expect("write");
            git(&main, &["add", "file.txt"]);
            git(&main, &["commit", "-m", "initial"]);
            git(&main, &["branch", "feature-a"]);
            git(&main, &["branch", "feature-b"]);
            Self { tmp, main }
        }

        fn worktree(&self, name: &str, branch: &str) -> PathBuf {
            let path = self.tmp.path().join(name);
            git(
                &self.main,
                &["worktree", "add", path.to_str().unwrap(), branch],
            );
            path
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command.args(args).current_dir(cwd);
        sanitize_git_environment(&mut command);
        let output = command
            .output()
            .expect("git available for repository tests");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn main_linked_subdir_and_origin_identity_contracts_hold() {
        let fixture = RepoFixture::new();
        git(
            &fixture.main,
            &["remote", "add", "origin", "git@github.com:User/My-Repo.git"],
        );
        let wt = fixture.worktree("wt space λ", "feature-a");
        let nested = wt.join("src").join("nested");
        fs::create_dir_all(&nested).unwrap();

        let main = discover_checkout(&fixture.main).unwrap().unwrap();
        let linked = discover_checkout(&wt).unwrap().unwrap();
        let nested = discover_checkout(&nested).unwrap().unwrap();

        assert_eq!(main.kind, CheckoutKind::Main);
        assert_eq!(linked.kind, CheckoutKind::Linked);
        assert!(same_repository(&main, &linked));
        assert_eq!(
            main.repository.repository_key,
            linked.repository.repository_key
        );
        assert_ne!(main.checkout_key, linked.checkout_key);
        assert_eq!(nested.checkout_key, linked.checkout_key);
        assert_eq!(linked.root, nested.root);
        assert_eq!(
            linked.repository.portable_identity,
            Some("git:github.com/user/my-repo".into())
        );
    }

    #[test]
    fn checkout_key_survives_detach_and_worktree_move() {
        let fixture = RepoFixture::new();
        let old = fixture.worktree("wt-a", "feature-a");
        let before = discover_checkout(&old).unwrap().unwrap();

        git(&old, &["switch", "--detach"]);
        let detached = discover_checkout(&old).unwrap().unwrap();
        assert_eq!(before.checkout_key, detached.checkout_key);
        assert_eq!(before.checkout_generation, detached.checkout_generation);
        assert!(detached.branch.is_none());

        let moved = fixture.tmp.path().join("wt-moved");
        git(
            &fixture.main,
            &[
                "worktree",
                "move",
                old.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );
        let after = discover_checkout(&moved).unwrap().unwrap();
        assert_eq!(before.checkout_key, after.checkout_key);
        assert_eq!(before.checkout_generation, after.checkout_generation);
        assert_ne!(before.root, after.root);

        let rebound = resolve_checkout_by_git_dir(&before.git_dir)
            .unwrap()
            .expect("stable worktree admin slot should resolve the moved checkout");
        assert_eq!(rebound.checkout_key, before.checkout_key);
        assert_eq!(rebound.root, moved.canonicalize().unwrap());
    }

    #[test]
    fn separate_clones_of_same_remote_are_not_same_local_repository() {
        let fixture = RepoFixture::new();
        git(
            &fixture.main,
            &["remote", "add", "origin", "git@github.com:Example/Repo.git"],
        );
        let clone = fixture.tmp.path().join("clone");
        git(
            fixture.tmp.path(),
            &[
                "clone",
                "--quiet",
                fixture.main.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git(
            &clone,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:Example/Repo.git",
            ],
        );

        let a = discover_checkout(&fixture.main).unwrap().unwrap();
        let b = discover_checkout(&clone).unwrap().unwrap();
        assert!(!same_repository(&a, &b));
        assert_ne!(a.repository.repository_key, b.repository.repository_key);
        assert_eq!(
            a.repository.portable_identity,
            Some("git:github.com/example/repo".into())
        );
        assert_eq!(
            a.repository.portable_identity,
            b.repository.portable_identity
        );
    }

    #[test]
    fn listing_reconciles_stale_active_and_reports_locked_sibling() {
        let fixture = RepoFixture::new();
        let old = fixture.main.join(".worktrees").join("wt-a");
        git(
            &fixture.main,
            &["worktree", "add", old.to_str().unwrap(), "feature-a"],
        );
        let locked = fixture.worktree("wt-b", "feature-b");
        git(
            &fixture.main,
            &["worktree", "lock", locked.to_str().unwrap()],
        );

        let before = discover_checkout(&old).unwrap().unwrap();
        let moved = fixture.tmp.path().join("wt-manually-moved");
        fs::rename(&old, &moved).expect("manual move");
        // Recreate the stale registered path inside the main checkout. A naive
        // discovery at that path now finds the enclosing main checkout.
        fs::create_dir_all(&old).expect("recreate stale path");

        let live = discover_checkout(&moved).unwrap().unwrap();
        assert_eq!(before.checkout_key, live.checkout_key);
        let list = list_worktrees(&live).unwrap();
        assert_eq!(list[0].root, live.root);
        assert_eq!(list[0].git_dir.as_ref(), Some(&live.git_dir));
        assert!(!list[0].prunable);
        assert!(
            list.iter()
                .skip(1)
                .all(|entry| entry.git_dir.as_ref() != Some(&live.git_dir)),
            "stale registration for active checkout must be reconciled"
        );
        assert!(
            list.iter().any(|entry| {
                entry.root == crate::core::pathutil::safe_canonicalize_or_self(&locked)
                    && entry.locked
            }),
            "locked sibling metadata must be preserved"
        );
    }

    #[test]
    fn removed_worktree_admin_slot_no_longer_resolves_checkout() {
        let fixture = RepoFixture::new();
        let wt = fixture.worktree("wt-remove", "feature-a");
        let identity = discover_checkout(&wt).unwrap().unwrap();

        git(&fixture.main, &["worktree", "remove", wt.to_str().unwrap()]);

        assert!(
            resolve_checkout_by_git_dir(&identity.git_dir)
                .unwrap()
                .is_none(),
            "removed worktree admin slot must not resurrect checkout authority"
        );
    }

    #[test]
    fn remove_and_readd_same_admin_path_gets_new_generation() {
        let fixture = RepoFixture::new();
        let wt = fixture.worktree("wt-generation", "feature-a");
        let before = discover_checkout(&wt).unwrap().unwrap();

        git(&fixture.main, &["worktree", "remove", wt.to_str().unwrap()]);
        git(
            &fixture.main,
            &["worktree", "add", wt.to_str().unwrap(), "feature-a"],
        );

        let after = discover_checkout(&wt).unwrap().unwrap();
        assert_eq!(
            before.git_dir, after.git_dir,
            "Git may reuse the same admin path after remove/re-add"
        );
        assert_eq!(before.checkout_key, after.checkout_key);
        assert_ne!(
            before.checkout_generation, after.checkout_generation,
            "a new admin-slot generation must not inherit old checkout authority"
        );
    }

    #[test]
    fn missing_registered_worktree_is_reported_as_prunable_metadata_only() {
        let fixture = RepoFixture::new();
        let wt = fixture.worktree("wt-prunable", "feature-a");
        fs::remove_dir_all(&wt).unwrap();

        let main = discover_checkout(&fixture.main).unwrap().unwrap();
        let listed = list_worktrees(&main).unwrap();
        let stale = listed
            .iter()
            .find(|entry| entry.root == wt)
            .expect("stale registered worktree should remain visible");
        assert!(!stale.exists);
        assert!(stale.prunable);
        assert!(stale.git_dir.is_some());
    }

    #[test]
    fn locked_missing_worktree_is_not_treated_as_live_authority() {
        let fixture = RepoFixture::new();
        let wt = fixture.worktree("wt-locked-missing", "feature-a");
        git(&fixture.main, &["worktree", "lock", wt.to_str().unwrap()]);
        fs::remove_dir_all(&wt).unwrap();

        let main = discover_checkout(&fixture.main).unwrap().unwrap();
        let listed = list_worktrees(&main).unwrap();
        let stale = listed
            .iter()
            .find(|entry| entry.root == wt)
            .expect("locked missing worktree should remain visible");
        assert!(!stale.exists);
        assert!(stale.locked);
    }

    #[test]
    fn non_repository_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        // The system temp directory can itself be a repository on developer
        // machines; an empty .git entry shadows any ambient parent repository.
        fs::create_dir(tmp.path().join(".git")).unwrap();
        assert!(discover_checkout(tmp.path()).unwrap().is_none());
    }
}
