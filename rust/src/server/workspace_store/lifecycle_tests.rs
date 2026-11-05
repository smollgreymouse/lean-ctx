// SPDX-License-Identifier: Apache-2.0
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
fn missing_legacy_additional_git_worktree_does_not_block_independent_git_addition() {
    let tmp = tempfile::tempdir().unwrap();
    let primary = tmp.path().join("primary");
    fs::create_dir(&primary).unwrap();
    let store = tmp.path().join("workspace.json");
    initialized_store(&store, primary.to_str().unwrap()).unwrap();

    // Reproduce a pre-checkout-binding Workspace entry: an older writer
    // persisted a linked Git worktree path in the legacy JSON without a
    // sidecar identity record.
    let stale_repo = RepoFixture::new();
    let stale = stale_repo.linked("stale-linked");
    let stale_text = stale.to_string_lossy().into_owned();
    let mut legacy = load(&store).unwrap();
    legacy.revision = 2;
    legacy.additional = vec![stale_text.clone()];
    fs::write(&store, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let _ = fs::remove_file(metadata_path(&store));
    git(
        &stale_repo.main,
        &["worktree", "remove", stale.to_str().unwrap()],
    );

    let candidate = RepoFixture::new();
    let candidate_text = candidate.main.to_string_lossy().into_owned();
    let added = mutate(&store, 2, candidate.main.to_str().unwrap(), true)
        .expect("missing legacy worktree must not block an unrelated Git root");
    assert_eq!(added.revision, 3);
    assert!(added.additional.contains(&candidate_text));
    assert!(added.additional.contains(&stale_text));

    let resolved = load_resolved(&store).unwrap();
    assert!(resolved.additional.iter().any(|status| {
        status.configured == stale_text
            && status.state == WorkspaceRootState::Missing
            && status.active.is_none()
    }));
    assert!(resolved.additional.iter().any(|status| {
        status.configured == candidate_text
            && status.state == WorkspaceRootState::Active
            && status.active.as_deref() == Some(candidate.main.as_path())
    }));

    let view = admin_view(&store).unwrap();
    assert!(view.additional_statuses.iter().any(|status| {
        status.configured == stale_text && status.state == WorkspaceRootState::Missing
    }));
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
    atomic_save_metadata_records(&anchor, &[old_metadata.clone(), new_metadata.clone()]).unwrap();
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
