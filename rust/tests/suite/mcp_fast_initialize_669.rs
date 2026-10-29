// SPDX-License-Identifier: Apache-2.0

//! Fast-initialize contract (GH #669).
//!
//! VS Code's start-on-demand MCP lifecycle races the first tool call of a
//! fresh conversation against server startup (microsoft/vscode#321150). The
//! server-side mitigation: nothing that spawns processes or opens sockets may
//! run in front of the `initialize` handshake — housekeeping (orphan sweep,
//! proxy autostart, publish throttle) is deferred onto the blocking pool.
//!
//! Two guards, real binary over real stdio JSON-RPC in an isolated HOME:
//!   1. spawn → initialize-response stays under a conservative wall-clock
//!      bound (catches a future re-introduction of synchronous pre-serve work
//!      such as a crash-backoff sleep or a network wait),
//!   2. a tools/call fired IMMEDIATELY after the initialized notification —
//!      the exact VS Code race pattern — succeeds on the first attempt.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::mcp_process as process;
use base64::Engine as _;

/// Generous for CI (debug binary, cold cache, shared runners) yet far below
/// the pathological regressions this guards against (30s crash-loop backoff,
/// TCP timeouts, N×ps orphan sweeps in front of the handshake).
const INITIALIZE_DEADLINE: Duration = Duration::from_secs(10);
const ICON_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/pS8AAAAASUVORK5CYII=";

struct TestEnv {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    data: std::path::PathBuf,
    state: std::path::PathBuf,
    project: std::path::PathBuf,
    icon: std::path::PathBuf,
}

fn test_env() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let state = tmp.path().join("state");
    let project = tmp.path().join("project");
    let icon = tmp.path().join("harr-test-icon.png");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::write(project.join("hello.txt"), "hello fast init\n").unwrap();
    std::fs::write(project.join("second.txt"), "second fast init\n").unwrap();
    std::fs::write(
        &icon,
        base64::engine::general_purpose::STANDARD
            .decode(ICON_PNG_BASE64)
            .expect("valid test PNG fixture"),
    )
    .unwrap();
    TestEnv {
        _tmp: tmp,
        home,
        data,
        state,
        project,
        icon,
    }
}

#[test]
#[cfg_attr(
    windows,
    ignore = "HOME-override isolation is Unix-only (dirs::home_dir uses the Win32 API)"
)]
fn initialize_answers_fast_and_first_call_succeeds() {
    let bin = env!("CARGO_BIN_EXE_lean-ctx");
    let env = test_env();

    let spawn_at = Instant::now();
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(&env.project)
        .env("HOME", &env.home)
        .env("LEAN_CTX_DATA_DIR", &env.data)
        .env("LEAN_CTX_STATE_DIR", &env.state)
        .env("HARR_MCP_ICON_PATH", &env.icon)
        .env("CODEX_HOME", env.home.join(".codex"))
        .env("LEAN_CTX_HEADLESS", "1")
        // Root detection must derive from the temp project's cwd. When the
        // suite itself runs inside an IDE/agent session these carry the HOST
        // workspace and would hijack the project root (→ path-jail rejects
        // the temp file).
        .env_remove("LEAN_CTX_PROJECT_ROOT")
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("WORKSPACE_FOLDER_PATHS")
        .env_remove("LEAN_CTX_WORKSPACE_FILE")
        .env_remove("LEAN_CTX_STRICT_WORKSPACE")
        .env_remove("LEAN_CTX_STRICT_WORKSPACE_LEGACY_TOOLS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("mcp server spawn");

    // Own cleanup before any assertion or fallible pipe setup can unwind.
    let mut process = process::ChildGuard::new(&mut child);
    let mut stdin = process.child_mut().stdin.take().expect("child stdin");
    let stdout = process.child_mut().stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let recv_response = |id: u64, deadline: Duration| -> serde_json::Value {
        let needle = format!("\"id\":{id}");
        let until = Instant::now() + deadline;
        loop {
            let remaining = until
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timeout waiting for JSON-RPC response id={id}"));
            let line = rx
                .recv_timeout(remaining)
                .unwrap_or_else(|e| panic!("no response id={id} within deadline: {e}"));
            if line.contains(&needle) {
                return serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("invalid JSON-RPC line: {e}\n{line}"));
            }
        }
    };

    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "vscode", "version": "1.109.0" }
        }
    });
    writeln!(stdin, "{init}").expect("write initialize");
    let init_res = recv_response(1, INITIALIZE_DEADLINE);
    let elapsed = spawn_at.elapsed();
    assert!(
        init_res["result"]["serverInfo"]["name"].is_string(),
        "initialize must return serverInfo; got: {init_res}"
    );
    let icons = init_res["result"]["serverInfo"]["icons"]
        .as_array()
        .expect("initialize serverInfo must advertise the configured icon");
    assert_eq!(icons.len(), 1, "the server should advertise one icon");
    assert_eq!(
        icons[0]["src"],
        format!("data:image/png;base64,{ICON_PNG_BASE64}"),
        "initialize must embed the PNG from HARR_MCP_ICON_PATH"
    );
    assert_eq!(icons[0]["mimeType"], "image/png");
    assert!(
        elapsed < INITIALIZE_DEADLINE,
        "spawn → initialize-response took {elapsed:?} (bound {INITIALIZE_DEADLINE:?}) — \
         synchronous pre-serve work crept back in front of the handshake (#669)"
    );

    // The VS Code race pattern: initialized notification and the first tool
    // call back-to-back, no grace period. It must succeed first try.
    writeln!(
        stdin,
        "{}",
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .expect("write initialized");
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {
            "name": "ctx_read",
            "arguments": { "path": env.project.join("hello.txt").to_string_lossy() }
        }
    });
    writeln!(stdin, "{call}").expect("write tools/call");
    let call_res = recv_response(2, Duration::from_secs(30));
    assert!(
        call_res["error"].is_null(),
        "first tools/call immediately after initialized must succeed; got: {call_res}"
    );
    let text = call_res["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("hello fast init"),
        "ctx_read must deliver the file on the first post-initialize call; got: {call_res}"
    );

    let second_call = serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {
            "name": "ctx_read",
            "arguments": { "path": env.project.join("second.txt").to_string_lossy() }
        }
    });
    writeln!(stdin, "{second_call}").expect("write second tools/call");
    let second_res = recv_response(3, Duration::from_secs(30));
    assert!(
        second_res["error"].is_null(),
        "second tools/call must succeed; got: {second_res}"
    );
    let second_text = second_res["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        second_text.contains("second fast init"),
        "ctx_read must deliver the second file; got: {second_res}"
    );

    let list = serde_json::json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/list", "params": {}
    });
    writeln!(stdin, "{list}").expect("write tools/list");
    let list_res = recv_response(4, Duration::from_secs(30));
    assert!(
        list_res["error"].is_null(),
        "tools/list after initialize and calls must succeed; got: {list_res}"
    );
    let tools = list_res["result"]["tools"]
        .as_array()
        .expect("tools/list must return an array");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .expect("every listed tool must have a name")
        })
        .collect();
    let unique_names: std::collections::HashSet<&str> = names.iter().copied().collect();
    assert_eq!(
        unique_names.len(),
        names.len(),
        "tools/list must not duplicate tool names"
    );
    let read_tool = tools
        .iter()
        .find(|tool| tool["name"] == "ctx_read")
        .expect("tools/list must preserve ctx_read");
    assert!(
        read_tool["inputSchema"]["properties"]["path"].is_object(),
        "ctx_read must preserve its path schema"
    );
    assert!(
        tools.iter().all(|tool| tool.get("icons").is_none()),
        "the server icon must not be repeated on individual tools"
    );

    drop(stdin); // EOF → clean server shutdown
    let status = process
        .wait_for_exit(Duration::from_secs(5))
        .expect("MCP server must exit after stdin EOF");
    assert!(status.success(), "MCP server shutdown failed: {status}");
    process::join_bounded(reader, Duration::from_secs(2))
        .expect("stdout reader must finish after server shutdown");

    let ledger_path = env.state.join("context_ledger.json");
    let ledger_text = std::fs::read_to_string(&ledger_path)
        .unwrap_or_else(|error| panic!("persisted ledger missing at {ledger_path:?}: {error}"));
    let ledger: serde_json::Value =
        serde_json::from_str(&ledger_text).expect("persisted ledger must be valid JSON");
    let entries = ledger["entries"]
        .as_array()
        .expect("persisted ledger entries must be an array");
    assert_eq!(entries.len(), 2, "both reads must persist one ledger entry");
    for name in ["hello.txt", "second.txt"] {
        let expected_path = std::fs::canonicalize(env.project.join(name))
            .expect("read source must canonicalize")
            .to_string_lossy()
            .to_string();
        let entry = entries
            .iter()
            .find(|entry| entry["path"].as_str() == Some(expected_path.as_str()))
            .unwrap_or_else(|| panic!("missing persisted ledger entry for {expected_path}"));
        assert!(
            entry["original_tokens"].as_u64().unwrap_or(0) > 0,
            "{name} must have positive original token count"
        );
        assert!(
            entry["sent_tokens"].as_u64().unwrap_or(0) > 0,
            "{name} must have positive sent token count"
        );
    }
    assert!(
        ledger["total_tokens_sent"].as_u64().unwrap_or(0) > 0,
        "persisted ledger must have positive aggregate sent tokens"
    );
}
