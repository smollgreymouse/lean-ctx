// SPDX-License-Identifier: Apache-2.0
use super::*;

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
