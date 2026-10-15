// SPDX-License-Identifier: Apache-2.0
//! Separate local administrative CLI. No management credentials or operations
//! are exposed to the MCP tool registry / prompt or to a remotely selected ID.
use std::path::Path;

#[cfg(debug_assertions)]
fn verify_same_process_isolation() -> Result<(), String> {
    use crate::core::pathjail::jail_path_with_roots;
    use crate::core::policy::runtime::{REQUEST_WORKSPACE, WorkspaceSnapshot};
    let fixture = tempfile::tempdir().map_err(|e| e.to_string())?;
    let a = fixture.path().join("a");
    let b = fixture.path().join("b");
    let c = fixture.path().join("c");
    for root in [&a, &b, &c] {
        std::fs::create_dir(root).map_err(|e| e.to_string())?;
        std::fs::write(root.join("data.txt"), "probe").map_err(|e| e.to_string())?;
    }
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|threads| {
        let barrier_a = barrier.clone();
        let a1 = a.clone();
        let b1 = b.clone();
        let c1 = c.clone();
        let first = threads.spawn(move || {
            let snapshot = WorkspaceSnapshot {
                primary_root: a1.clone(),
                additional_roots: vec![b1.clone()],
                revision: 3,
            };
            REQUEST_WORKSPACE.sync_scope(Some(snapshot), || {
                barrier_a.wait();
                jail_path_with_roots(&b1.join("data.txt"), &a1, &[]).is_ok()
                    && jail_path_with_roots(&c1.join("data.txt"), &a1, &[]).is_err()
            })
        });
        let c2 = c.clone();
        let b2 = b.clone();
        let second = threads.spawn(move || {
            let snapshot = WorkspaceSnapshot {
                primary_root: c2.clone(),
                additional_roots: Vec::new(),
                revision: 9,
            };
            REQUEST_WORKSPACE.sync_scope(Some(snapshot), || {
                barrier.wait();
                jail_path_with_roots(&c2.join("data.txt"), &c2, &[]).is_ok()
                    && jail_path_with_roots(&b2.join("data.txt"), &c2, &[]).is_err()
            })
        });
        if first.join().unwrap_or(false) && second.join().unwrap_or(false) {
            Ok(())
        } else {
            Err("Request-scoped PathJail authorities overlapped".into())
        }
    })
}

pub(crate) fn run(args: &[String]) -> i32 {
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("self-test-isolation") {
        return match verify_same_process_isolation() {
            Ok(()) => {
                println!("PASS: concurrent workspace request scopes are isolated");
                0
            }
            Err(error) => {
                eprintln!("FAIL: {error}");
                1
            }
        };
    }
    let Some(operation) = args.first().map(String::as_str) else {
        eprintln!(
            "Usage: lean-ctx workspace-admin <create|show|add|remove> <absolute-store-path> [root] [expected-revision]"
        );
        return 2;
    };
    let Some(store) = args.get(1) else {
        eprintln!("An explicit absolute store path is required");
        return 2;
    };
    let path = Path::new(store);
    let result = match operation {
        "show" if args.len() == 2 => crate::server::workspace_store::load(path),
        "create" if args.len() == 3 => {
            crate::server::workspace_store::initialized_store(path, &args[2])
                .and_then(|_| crate::server::workspace_store::load(path))
        }
        "add" | "remove" if args.len() == 4 => {
            let revision = match args[3].parse::<u64>() {
                Ok(revision) if revision > 0 => revision,
                _ => {
                    eprintln!("Expected positive numeric revision");
                    return 2;
                }
            };
            crate::server::workspace_store::mutate(path, revision, &args[2], operation == "add")
        }
        _ => {
            eprintln!("Invalid workspace-admin operation or arguments");
            return 2;
        }
    };
    match result {
        Ok(state) => match serde_json::to_string(&state) {
            Ok(data) => {
                println!("{data}");
                0
            }
            Err(_) => 1,
        },
        Err(error) => {
            eprintln!("Workspace admin failed: {error}");
            1
        }
    }
}
