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
                workspace_id: None,
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
                workspace_id: None,
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

#[cfg(debug_assertions)]
fn verify_long_lived_mcp_sessions() -> Result<(), String> {
    use crate::tools::LeanCtxServer;
    use rmcp::{ServiceExt, model::CallToolRequestParams};
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let primary = tmp.path().join("primary");
    let extra_a = tmp.path().join("extra-a");
    let extra_b = tmp.path().join("extra-b");
    for (path, marker) in [
        (&primary, "primary_shared_marker"),
        (&extra_a, "long_lived_A_secret_91"),
        (&extra_b, "long_lived_B_secret_73"),
    ] {
        std::fs::create_dir(path).map_err(|e| e.to_string())?;
        std::fs::create_dir(path.join(".git")).map_err(|e| e.to_string())?;
        std::fs::write(path.join("unique.txt"), marker).map_err(|e| e.to_string())?;
    }
    let store_a = tmp.path().join("A.json");
    let store_b = tmp.path().join("B.json");
    crate::server::workspace_store::initialized_store(&store_a, primary.to_str().unwrap())?;
    crate::server::workspace_store::initialized_store(&store_b, primary.to_str().unwrap())?;
    crate::server::workspace_store::mutate(&store_a, 1, extra_a.to_str().unwrap(), true)?;
    crate::server::workspace_store::mutate(&store_b, 1, extra_b.to_str().unwrap(), true)?;
    // Long-lived transport isolation is tested independently of shared SessionMode;
    // shared SessionState collision has its own dedicated self-test below.
    let mut server_a = LeanCtxServer::new_with_project_root(primary.to_str());
    let mut server_b = LeanCtxServer::new_with_project_root(primary.to_str());
    server_a.workspace_store_path = Some(store_a);
    server_b.workspace_store_path = Some(store_b);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let (client_io_a, server_io_a) = tokio::io::duplex(64 * 1024);
        let (client_io_b, server_io_b) = tokio::io::duplex(64 * 1024);
        let (srv_a, cli_a, srv_b, cli_b) = tokio::join!(
            server_a.serve(server_io_a),
            ().serve(client_io_a),
            server_b.serve(server_io_b),
            ().serve(client_io_b),
        );
        let mut srv_a = srv_a.map_err(|e| format!("server A init: {e:?}"))?;
        let mut cli_a = cli_a.map_err(|e| format!("client A init: {e:?}"))?;
        let mut srv_b = srv_b.map_err(|e| format!("server B init: {e:?}"))?;
        let mut cli_b = cli_b.map_err(|e| format!("client B init: {e:?}"))?;

        let call = |name: &str, path: Option<&std::path::Path>| {
            let mut args = serde_json::Map::new();
            if let Some(path) = path {
                args.insert("path".into(), serde_json::json!(path.to_string_lossy()));
            }
            if name == "ctx_read" {
                args.insert("fresh".into(), serde_json::json!(true));
            }
            CallToolRequestParams::new(name.to_string()).with_arguments(args)
        };

        for _ in 0..50 {
            let (a_get, b_get, a_read, b_read) = tokio::join!(
                cli_a.peer().call_tool(call("ctx_workspace_get", None)),
                cli_b.peer().call_tool(call("ctx_workspace_get", None)),
                cli_a.peer().call_tool(call("ctx_read", Some(&extra_a.join("unique.txt")))),
                cli_b.peer().call_tool(call("ctx_read", Some(&extra_a.join("unique.txt")))),
            );
            let a_get = serde_json::to_string(&a_get.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let b_get = serde_json::to_string(&b_get.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let a_read = serde_json::to_string(&a_read.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let b_read = serde_json::to_string(&b_read.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            if !a_get.contains("extra-a") || a_get.contains("extra-b")
                || !b_get.contains("extra-b") || b_get.contains("extra-a")
                || !a_read.contains("long_lived_A_secret_91")
                || b_read.contains("long_lived_A_secret_91")
            {
                return Err(format!("long-lived MCP authority crossed: Aget={a_get}; Bget={b_get}; Aread={a_read}; Bread={b_read}"));
            }
        }
        cli_a.close().await.map_err(|e| e.to_string())?;
        cli_b.close().await.map_err(|e| e.to_string())?;
        srv_a.close().await.map_err(|e| e.to_string())?;
        srv_b.close().await.map_err(|e| e.to_string())?;
        Ok(())
    })
}

pub(crate) fn run(args: &[String]) -> i32 {
    #[cfg(debug_assertions)]
    if args
        .first()
        .is_some_and(|arg| arg.starts_with("self-test-"))
    {
        // Security self-tests must never run startup maintenance against the
        // developer's real HOME (IDE hooks/rules, skills, etc.). This command is
        // a short-lived standalone process, so changing its environment is safe.
        unsafe { std::env::set_var("LEAN_CTX_HEADLESS", "1") };
    }
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("self-test-long-lived-mcp") {
        return match verify_long_lived_mcp_sessions() {
            Ok(()) => {
                println!("PASS: long-lived duplex MCP sessions keep Workspace authority isolated");
                0
            }
            Err(error) => {
                eprintln!("FAIL: {error}");
                1
            }
        };
    }
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("self-test-progress-routing") {
        return match verify_progress_routing_is_request_local() {
            Ok(()) => {
                println!("PASS: overlapping MCP progress notifications stay request-local");
                0
            }
            Err(error) => {
                eprintln!("FAIL: {error}");
                1
            }
        };
    }
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("self-test-shared-session") {
        return match verify_shared_session_workspace_isolation() {
            Ok(()) => {
                println!("PASS: shared SessionState does not merge Workspace authority");
                0
            }
            Err(error) => {
                eprintln!("FAIL: {error}");
                1
            }
        };
    }
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("self-test-bm25-revoke") {
        return match verify_bm25_revocation_during_build() {
            Ok(()) => {
                println!("PASS: BM25 build result suppressed after mid-build revocation");
                0
            }
            Err(error) => {
                eprintln!("FAIL: {error}");
                1
            }
        };
    }
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("self-test-dense-namespace") {
        return match verify_dense_namespace_is_workspace_scoped() {
            Ok(()) => {
                println!("PASS: dense index namespace includes Workspace identity and revision");
                0
            }
            Err(error) => {
                eprintln!("FAIL: {error}");
                1
            }
        };
    }
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("self-test-ann-cache") {
        return match crate::core::ann_cache::debug_adversarial_fingerprint_self_test() {
            Ok(()) => {
                println!("PASS: ANN cache distinguishes adversarial corpora");
                0
            }
            Err(error) => {
                eprintln!("FAIL: {error}");
                1
            }
        };
    }
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("self-test-mcp-dual") {
        return match verify_dual_mcp_handler_isolation() {
            Ok(()) => {
                println!("PASS: two concurrent MCP handlers have distinct Workspace authority");
                0
            }
            Err(error) => {
                eprintln!("FAIL: {error}");
                1
            }
        };
    }
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

#[cfg(debug_assertions)]
fn verify_dual_mcp_handler_isolation() -> Result<(), String> {
    use crate::engine::ContextEngine;
    use crate::tools::LeanCtxServer;
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let ra = tmp.path().join("repo-a");
    let rb = tmp.path().join("repo-b");
    let rc = tmp.path().join("repo-c");
    for (path, marker) in [
        (&ra, "orion_cobalt_A_secret"),
        (&rb, "violet_amber_B_secret"),
        (&rc, "silver_C_secret"),
    ] {
        std::fs::create_dir(path).map_err(|e| e.to_string())?;
        std::fs::create_dir(path.join(".git")).map_err(|e| e.to_string())?;
        std::fs::write(path.join("unique.txt"), marker).map_err(|e| e.to_string())?;
    }
    let store_a = tmp.path().join("workspace-A.json");
    let store_c = tmp.path().join("workspace-C.json");
    crate::server::workspace_store::initialized_store(&store_a, ra.to_str().unwrap())?;
    crate::server::workspace_store::initialized_store(&store_c, rc.to_str().unwrap())?;
    crate::server::workspace_store::mutate(&store_a, 1, rb.to_str().unwrap(), true)?;
    let mut server_a = LeanCtxServer::new_with_project_root(ra.to_str());
    let mut server_c = LeanCtxServer::new_with_project_root(rc.to_str());
    server_a.workspace_store_path = Some(store_a);
    server_c.workspace_store_path = Some(store_c);
    let engine_a = ContextEngine::from_server(server_a);
    let engine_c = ContextEngine::from_server(server_c);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let (a, c) = tokio::join!(
            engine_a.call_tool_value("ctx_workspace_get", None),
            engine_c.call_tool_value("ctx_workspace_get", None),
        );
        let a = a.map_err(|e| e.to_string())?.to_string();
        let c = c.map_err(|e| e.to_string())?.to_string();
        if !a.contains("repo-b")
            || c.contains("repo-b")
            || a.contains("repo-c")
            || !c.contains("repo-c")
        {
            return Err(format!("Workspace identity leak: A={a}, C={c}"));
        }
        let (allowed, forbidden) = tokio::join!(
            engine_a.call_tool_value(
                "ctx_read",
                Some(serde_json::json!({"path":rb.join("unique.txt").to_string_lossy()}))
            ),
            engine_c.call_tool_value(
                "ctx_read",
                Some(serde_json::json!({"path":rb.join("unique.txt").to_string_lossy()}))
            ),
        );
        let allowed = allowed.map_err(|e| e.to_string())?.to_string();
        let forbidden = forbidden.map_err(|e| e.to_string())?.to_string();
        if !allowed.contains("violet_amber_B_secret") || forbidden.contains("violet_amber_B_secret")
        {
            return Err(format!(
                "MCP handler root isolation violated: allowed={allowed}; denied={forbidden}"
            ));
        }
        Ok(())
    })
}

#[cfg(debug_assertions)]
fn verify_shared_session_workspace_isolation() -> Result<(), String> {
    use crate::engine::ContextEngine;
    use crate::tools::LeanCtxServer;
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let primary = tmp.path().join("primary");
    let extra = tmp.path().join("extra");
    std::fs::create_dir(&primary).map_err(|e| e.to_string())?;
    std::fs::create_dir(&extra).map_err(|e| e.to_string())?;
    std::fs::create_dir(primary.join(".git")).map_err(|e| e.to_string())?;
    std::fs::create_dir(extra.join(".git")).map_err(|e| e.to_string())?;
    std::fs::write(extra.join("secret.txt"), "shared_session_extra_secret_42")
        .map_err(|e| e.to_string())?;
    let store_a = tmp.path().join("A.json");
    let store_b = tmp.path().join("B.json");
    crate::server::workspace_store::initialized_store(&store_a, primary.to_str().unwrap())?;
    crate::server::workspace_store::initialized_store(&store_b, primary.to_str().unwrap())?;
    crate::server::workspace_store::mutate(&store_a, 1, extra.to_str().unwrap(), true)?;
    let mut sa = LeanCtxServer::new_shared_with_context(
        primary.to_str().unwrap(),
        "shared-security",
        "same-channel",
    );
    let mut sb = LeanCtxServer::new_shared_with_context(
        primary.to_str().unwrap(),
        "shared-security",
        "same-channel",
    );
    if !std::sync::Arc::ptr_eq(&sa.session, &sb.session) {
        return Err("fixture did not reuse shared SessionState".into());
    }
    sa.workspace_store_path = Some(store_a);
    sb.workspace_store_path = Some(store_b);
    let ea = ContextEngine::from_server(sa);
    let eb = ContextEngine::from_server(sb);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let args = Some(serde_json::json!({"path": extra.join("secret.txt").to_string_lossy()}));
        let (allowed, denied) = tokio::join!(
            ea.call_tool_value("ctx_read", args.clone()),
            eb.call_tool_value("ctx_read", args)
        );
        let allowed = allowed.map_err(|e| e.to_string())?.to_string();
        let denied = denied.map_err(|e| e.to_string())?.to_string();
        if !allowed.contains("shared_session_extra_secret_42")
            || denied.contains("shared_session_extra_secret_42")
        {
            return Err(format!(
                "shared SessionState leaked authority: allowed={allowed}; denied={denied}"
            ));
        }
        Ok(())
    })
}

#[cfg(debug_assertions)]
fn verify_bm25_revocation_during_build() -> Result<(), String> {
    use crate::engine::ContextEngine;
    use crate::tools::LeanCtxServer;
    use std::sync::Arc;
    use std::time::Duration;
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let primary = tmp.path().join("primary");
    let extra = tmp.path().join("extra");
    std::fs::create_dir(&primary).map_err(|e| e.to_string())?;
    std::fs::create_dir(&extra).map_err(|e| e.to_string())?;
    std::fs::create_dir(primary.join(".git")).map_err(|e| e.to_string())?;
    std::fs::create_dir(extra.join(".git")).map_err(|e| e.to_string())?;
    for i in 0..32 {
        std::fs::write(
            extra.join(format!("f{i}.rs")),
            format!("fn bm25_revocation_marker_{i}() {{}}\n"),
        )
        .map_err(|e| e.to_string())?;
    }
    let store = tmp.path().join("workspace.json");
    crate::server::workspace_store::initialized_store(&store, primary.to_str().unwrap())?;
    crate::server::workspace_store::mutate(&store, 1, extra.to_str().unwrap(), true)?;
    let mut server = LeanCtxServer::new_with_project_root(primary.to_str());
    server.workspace_store_path = Some(store.clone());
    let engine = Arc::new(ContextEngine::from_server(server));
    crate::server::workspace_test_hooks::arm("bm25_before_source_read");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(async move {
        let runner = engine.clone();
        let path = extra.clone();
        let search = tokio::spawn(async move {
            runner
                .call_tool_value(
                    "ctx_search",
                    Some(serde_json::json!({
                        "query":"bm25_revocation_marker_7", "path":path.to_string_lossy()
                    })),
                )
                .await
        });
        let reached = tokio::task::spawn_blocking(|| {
            crate::server::workspace_test_hooks::wait_reached(Duration::from_secs(20))
        })
        .await
        .map_err(|e| e.to_string())?;
        if !reached {
            crate::server::workspace_test_hooks::clear();
            return Err("BM25 barrier not reached".into());
        }
        let changed =
            crate::server::workspace_store::mutate(&store, 2, extra.to_str().unwrap(), false)?;
        if changed.revision != 3 {
            return Err("unexpected revocation revision".into());
        }
        crate::server::workspace_test_hooks::release();
        let response = search.await.map_err(|e| e.to_string())?;
        crate::server::workspace_test_hooks::clear();
        if response.is_ok() {
            return Err(format!(
                "stale BM25 result published after revocation: {response:?}"
            ));
        }
        Ok(())
    });
    result
}

#[cfg(debug_assertions)]
fn verify_progress_routing_is_request_local() -> Result<(), String> {
    use crate::engine::ContextEngine;
    use crate::tools::LeanCtxServer;
    use std::sync::Arc;
    use std::time::Duration;
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let root = tmp.path().join("repo");
    std::fs::create_dir(&root).map_err(|e| e.to_string())?;
    std::fs::create_dir(root.join(".git")).map_err(|e| e.to_string())?;
    std::fs::write(root.join("a.rs"), "fn progress_alpha_marker() {}\n")
        .map_err(|e| e.to_string())?;
    std::fs::write(root.join("b.rs"), "fn progress_beta_marker() {}\n")
        .map_err(|e| e.to_string())?;
    let server = LeanCtxServer::new_with_project_root(root.to_str());
    let a = Arc::new(ContextEngine::from_server(server.clone()));
    let b = Arc::new(ContextEngine::from_server(server));
    crate::server::workspace_test_hooks::arm("progress_after_start");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let outcome =
        rt.block_on(async move {
            let aa = a.clone();
            let bb = b.clone();
            let root_a = root.clone();
            let root_b = root.clone();
            let fa = tokio::spawn(async move {
                aa.call_tool_trace("ctx_semantic_search", Some(serde_json::json!({
                "query":"progress_alpha_marker", "path":root_a.to_string_lossy(), "mode":"bm25"
            })), "progress-A").await
            });
            let fb = tokio::spawn(async move {
                bb.call_tool_trace("ctx_semantic_search", Some(serde_json::json!({
                "query":"progress_beta_marker", "path":root_b.to_string_lossy(), "mode":"bm25"
            })), "progress-B").await
            });
            let reached = tokio::task::spawn_blocking(|| {
                crate::server::workspace_test_hooks::wait_hits(2, Duration::from_secs(20))
            })
            .await
            .map_err(|e| e.to_string())?;
            if !reached {
                crate::server::workspace_test_hooks::clear();
                return Err("two overlapping progress requests did not reach barrier".into());
            }
            crate::server::workspace_test_hooks::release();
            let (_, notes_a) = fa
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            let (_, notes_b) = fb
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            crate::server::workspace_test_hooks::clear();
            let joined_a = notes_a.join("\n");
            let joined_b = notes_b.join("\n");
            if !joined_a.contains("progress-A") || joined_a.contains("progress-B") {
                return Err(format!(
                    "peer A progress routing crossed requests: {joined_a}"
                ));
            }
            if !joined_b.contains("progress-B") || joined_b.contains("progress-A") {
                return Err(format!(
                    "peer B progress routing crossed requests: {joined_b}"
                ));
            }
            // Scope cleanup: after both overlapping requests complete, a third
            // call must route only to its own peer/token, never a stale sender.
            let (_, notes_c) = a
            .call_tool_trace(
                "ctx_semantic_search",
                Some(serde_json::json!({
                    "query":"progress_alpha_marker", "path":root.to_string_lossy(), "mode":"bm25"
                })),
                "progress-C",
            )
            .await
            .map_err(|e| e.to_string())?;
            let joined_c = notes_c.join("\n");
            if !joined_c.contains("progress-C")
                || joined_c.contains("progress-A")
                || joined_c.contains("progress-B")
            {
                return Err(format!(
                    "request-local progress sender survived scope cleanup: {joined_c}"
                ));
            }
            Ok(())
        });
    outcome
}

#[cfg(debug_assertions)]
fn verify_dense_namespace_is_workspace_scoped() -> Result<(), String> {
    use crate::core::policy::runtime::{REQUEST_WORKSPACE, WorkspaceSnapshot};
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    std::fs::create_dir(root.path().join(".git")).map_err(|e| e.to_string())?;
    let mk = |id: &str, revision: u64| WorkspaceSnapshot {
        workspace_id: Some(id.to_string()),
        primary_root: root.path().to_path_buf(),
        additional_roots: Vec::new(),
        revision,
    };
    let data = root.path().join("data");
    std::fs::create_dir(&data).map_err(|e| e.to_string())?;
    unsafe { std::env::set_var("LEAN_CTX_DATA_DIR", &data) };
    let scoped = |snapshot: WorkspaceSnapshot| {
        REQUEST_WORKSPACE.sync_scope(Some(snapshot), || {
            let hash = crate::core::index_namespace::namespace_hash(root.path());
            let dir = crate::core::index_namespace::vectors_dir(root.path());
            (hash, dir)
        })
    };
    let (a, dir_a) = scoped(mk("11111111-1111-4111-8111-111111111111", 1));
    let (b, dir_b) = scoped(mk("22222222-2222-4222-8222-222222222222", 1));
    let (a2, dir_a2) = scoped(mk("11111111-1111-4111-8111-111111111111", 2));
    if a == b || a == a2 || b == a2 || dir_a == dir_b || dir_a == dir_a2 || dir_b == dir_a2 {
        return Err(format!(
            "dense namespace collision: A={a}:{dir_a:?}, B={b}:{dir_b:?}, A2={a2}:{dir_a2:?}"
        ));
    }
    std::fs::create_dir_all(&dir_a).map_err(|e| e.to_string())?;
    std::fs::write(dir_a.join("workspace-A-only.marker"), b"A").map_err(|e| e.to_string())?;
    if dir_b.join("workspace-A-only.marker").exists()
        || dir_a2.join("workspace-A-only.marker").exists()
    {
        return Err("persisted dense namespace leaked across Workspace identity/revision".into());
    }

    // Exercise the real persisted EmbeddingIndex save/load path without loading
    // an embedding model. Same primary root, same data dir; only Workspace
    // identity/revision may separate the stored vector corpus.
    REQUEST_WORKSPACE.sync_scope(Some(mk("11111111-1111-4111-8111-111111111111", 1)), || {
        crate::core::embedding_index::EmbeddingIndex::new_with_model(4, "workspace-A-model")
            .save(root.path())
            .map_err(|e| e.to_string())
    })?;
    let a_loaded = REQUEST_WORKSPACE
        .sync_scope(Some(mk("11111111-1111-4111-8111-111111111111", 1)), || {
            crate::core::embedding_index::EmbeddingIndex::load(root.path())
        });
    let b_loaded = REQUEST_WORKSPACE
        .sync_scope(Some(mk("22222222-2222-4222-8222-222222222222", 1)), || {
            crate::core::embedding_index::EmbeddingIndex::load(root.path())
        });
    let a2_loaded = REQUEST_WORKSPACE
        .sync_scope(Some(mk("11111111-1111-4111-8111-111111111111", 2)), || {
            crate::core::embedding_index::EmbeddingIndex::load(root.path())
        });
    if a_loaded.and_then(|idx| idx.model_id).as_deref() != Some("workspace-A-model")
        || b_loaded.is_some()
        || a2_loaded.is_some()
    {
        return Err("persisted EmbeddingIndex crossed Workspace identity/revision".into());
    }
    Ok(())
}
