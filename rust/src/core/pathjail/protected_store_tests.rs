// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn runtime_store_is_not_reopened_by_implicit_or_explicit_roots() {
    let isolated = crate::core::data_dir::isolated_data_dir();
    let project = tempfile::tempdir().unwrap();
    let store = isolated.path().join("knowledge");
    std::fs::create_dir_all(&store).unwrap();
    let fact = store.join("facts.json");
    std::fs::write(&fact, "private stored context").unwrap();
    let code = project.path().join("main.rs");
    std::fs::write(&code, "fn main() {}\n").unwrap();
    assert!(
        jail_path(&fact, project.path()).is_ok(),
        "Community keeps its existing runtime access"
    );
    let _protected = crate::cli::pin_synthetic_session(isolated.path()).unwrap();
    let roots = vec![isolated.path().to_string_lossy().into_owned()];
    for candidate in [&fact, &store, &isolated.path().to_path_buf()] {
        assert!(
            matches!(
                jail_path_with_roots(candidate, project.path(), &roots),
                Err(PathJailError::ProtectedContext { .. })
            ),
            "{candidate:?}"
        );
    }
    assert!(jail_path(&code, project.path()).is_ok());
    assert!(jail_path_with_roots(Path::new("facts.json"), &store, &roots).is_err());
    // Raw backup paths enter the shared atomic writer without the dispatch
    // resolver; they must not overwrite a hidden record or create a sibling.
    for target in [&fact, &store.join("new/backup.json")] {
        assert!(
            crate::tools::edit_io::write_atomic_bytes_with_permissions(
                target,
                b"forged local context",
                None,
            )
            .is_err()
        );
    }
    assert_eq!(
        std::fs::read_to_string(&fact).unwrap(),
        "private stored context"
    );
    assert!(!store.join("new").exists());
    crate::tools::edit_io::write_atomic_bytes_with_permissions(&code, b"fn main() {}\n", None)
        .unwrap();
    // Internal storage is unaffected by the model-directed path resolver.
    std::fs::write(&fact, "retained internal update").unwrap();
    assert_eq!(
        std::fs::read_to_string(&fact).unwrap(),
        "retained internal update"
    );
}

#[test]
fn missing_child_profile_never_falls_back_to_the_normal_jail() {
    let isolated = crate::core::data_dir::isolated_data_dir();
    let project = tempfile::tempdir().unwrap();
    let code = project.path().join("main.rs");
    std::fs::write(&code, "fn main() {}\n").unwrap();
    let _protected = crate::cli::pin_synthetic_session(isolated.path()).unwrap();
    crate::test_env::remove_var(crate::cli::CHILD_PROFILE_ENV);
    assert!(matches!(
        jail_path(&code, project.path()),
        Err(PathJailError::ProtectedContext { .. })
    ));
}
