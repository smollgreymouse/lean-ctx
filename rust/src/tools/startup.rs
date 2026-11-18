use super::server::LeanCtxServer;

#[derive(Clone, Debug, Default)]
pub(super) struct StartupContext {
    pub(super) project_root: Option<String>,
    pub(super) shell_cwd: Option<String>,
}

/// Creates a new `LeanCtxServer` with default configuration.
pub fn create_server() -> LeanCtxServer {
    LeanCtxServer::new()
}

pub(super) fn has_project_marker(dir: &std::path::Path) -> bool {
    crate::core::pathutil::has_project_marker(dir)
}

pub(crate) fn is_suspicious_root(dir: &std::path::Path) -> bool {
    crate::core::pathutil::is_broad_or_unsafe_root(dir)
        || crate::core::pathutil::is_agent_config_dir(dir)
}

pub(super) fn canonicalize_path(path: &std::path::Path) -> String {
    crate::core::pathutil::safe_canonicalize_or_self(path)
        .to_string_lossy()
        .to_string()
}

pub(super) fn detect_startup_context(
    explicit_project_root: Option<&str>,
    startup_cwd: Option<&std::path::Path>,
    authority_mode: crate::core::authority_mode::AuthorityMode,
) -> StartupContext {
    // Rootless (`none`) derives nothing: the process CWD, an explicit root
    // argument or a project marker scan must never mint session authority.
    // A launcher-bound root arrives later through the Workspace store path.
    if authority_mode.is_rootless() {
        return StartupContext::default();
    }
    let shell_cwd = startup_cwd.map(canonicalize_path);
    let project_root = explicit_project_root
        .map(|root| canonicalize_path(std::path::Path::new(root)))
        .or_else(|| {
            startup_cwd
                .and_then(maybe_derive_project_root_from_absolute)
                .map(|p| canonicalize_path(&p))
        });

    let shell_cwd = match (shell_cwd, project_root.as_ref()) {
        (Some(cwd), Some(root))
            if std::path::Path::new(&cwd).starts_with(std::path::Path::new(root)) =>
        {
            Some(cwd)
        }
        (_, Some(root)) => Some(root.clone()),
        (cwd, None) => cwd,
    };

    StartupContext {
        project_root,
        shell_cwd,
    }
}

pub(super) fn maybe_derive_project_root_from_absolute(
    abs: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let mut cur = if abs.is_dir() {
        abs.to_path_buf()
    } else {
        abs.parent()?.to_path_buf()
    };
    // #1437: keep walking up and return the OUTERMOST project marker, matching
    // `detect_project_root`. The old "first match = return" behaviour caused
    // monorepo sub-packages (own `package.json`, no `.git`) to become the
    // derived root, which then poisoned the daemon session for all later queries.
    let mut best: Option<std::path::PathBuf> = None;
    loop {
        if has_project_marker(&cur) {
            best = Some(crate::core::pathutil::safe_canonicalize_or_self(&cur));
        }
        if !cur.pop() {
            break;
        }
    }
    best
}

/// Incremental background consolidation: import only session items newer than the
/// per-session watermark, advancing it after a successful save. Delegates to the
/// canonical engine ([`crate::tools::ctx_knowledge::consolidate_project_knowledge_with`]),
/// which loads the session for the *requested* project root (cwd bug #2362), runs
/// under the shared knowledge lock (#326) and reclaims history losslessly (#995).
pub(crate) fn auto_consolidate_knowledge(project_root: &str) {
    let _ = crate::tools::ctx_knowledge::consolidate_project_knowledge_with(
        project_root,
        &crate::core::consolidation_engine::ConsolidateOptions::incremental_auto(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_root_promotes_to_outermost_marker_in_monorepo() {
        // #1437: in a monorepo where sub-packages carry their own package.json,
        // the derived root must be the outermost marker (repo root with .git),
        // not the nearest sub-package marker.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let sub = repo.join("modules").join("payroll");
        std::fs::create_dir_all(&sub).unwrap();
        // Repo root has .git + package.json
        std::fs::create_dir(repo.join(".git")).unwrap();
        std::fs::write(repo.join("package.json"), "{}").unwrap();
        // Sub-package has only package.json (no .git)
        std::fs::write(sub.join("package.json"), "{}").unwrap();

        let result = maybe_derive_project_root_from_absolute(&sub);
        assert!(result.is_some(), "should find a root");
        let root = result.unwrap();
        let canonical_repo = crate::core::pathutil::safe_canonicalize_or_self(&repo);
        assert_eq!(
            root, canonical_repo,
            "should return repo root, not sub-package"
        );
    }

    #[test]
    fn derive_root_returns_nearest_when_no_outer_marker() {
        // When there is no outer marker (standalone sub-package), the nearest
        // marker is the correct root.
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("standalone");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), "{}").unwrap();

        let result = maybe_derive_project_root_from_absolute(&pkg);
        assert!(result.is_some());
        let root = result.unwrap();
        let canonical = crate::core::pathutil::safe_canonicalize_or_self(&pkg);
        assert_eq!(root, canonical);
    }

    /// Rootless startup derives nothing: the process cwd and any explicit root
    /// argument must not mint project or shell authority in `none` mode.
    #[test]
    fn rootless_startup_context_is_empty() {
        use crate::core::authority_mode::AuthorityMode;

        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();

        // Legacy keeps the existing derivation: the derived root is the
        // outermost project marker, which on some hosts legitimately sits
        // above the tempdir (e.g. a `.git` directly in TMPDIR).
        let derived = detect_startup_context(None, Some(&project), AuthorityMode::Legacy);
        assert_eq!(
            derived.project_root,
            maybe_derive_project_root_from_absolute(&project).map(|p| canonicalize_path(&p)),
        );
        assert!(derived.shell_cwd.is_some());

        let rootless = detect_startup_context(None, Some(&project), AuthorityMode::None);
        assert_eq!(rootless.project_root, None);
        assert_eq!(rootless.shell_cwd, None);

        // Even an explicit root argument cannot create authority in none mode:
        // a launcher-bound root arrives only through the Workspace store path.
        let explicit = detect_startup_context(
            Some(project.to_str().unwrap()),
            Some(&project),
            AuthorityMode::None,
        );
        assert_eq!(explicit.project_root, None);
        assert_eq!(explicit.shell_cwd, None);
    }
}
