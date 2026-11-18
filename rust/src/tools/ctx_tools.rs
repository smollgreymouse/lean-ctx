//! `ctx_tools` business logic (#210): the MCP Tool-Catalog meta-tool.
//!
//! Keeps the registered wrapper thin: this module owns config gating, action
//! routing, and driving the async [`mcp_catalog`] from the synchronous tool
//! handler. The MCP dispatch layer wraps handlers in `block_in_place`, so an ambient
//! runtime handle is available there. The CLI `call` path has no ambient
//! runtime, so `run` falls back to building its own current_thread runtime
//! (see `Rt`) instead of panicking on `Handle::current()`.

use std::sync::OnceLock;

use serde_json::{Map, Value};

use crate::core::config::Config;
use crate::core::mcp_catalog;

const DISABLED_HINT: &str = "gateway is disabled. Enable it in ~/.lean-ctx/config.toml:\n\
     [gateway]\n\
     enabled = true\n\n\
     [[gateway.servers]]\n\
     name = \"fs\"\n\
     transport = \"stdio\"\n\
     command = \"mcp-server-filesystem\"\n\
     args = [\"/path/to/dir\"]";

/// Process-lifetime runtime for gateway async calls.
///
/// Tool handlers run inside `spawn_blocking` (#1018), so the ambient server
/// runtime cannot be entered with `Handle::block_on`. A fresh runtime per call
/// is also incorrect: pooled downstream MCP sessions spawn background tasks on
/// that runtime, and dropping it immediately after the call leaves stale
/// `RunningService` objects in the global pool. Keep one dedicated multi-thread
/// runtime alive for the process instead, so pooled stdio/HTTP sessions really
/// survive across `ctx_tools` calls.
struct Rt(&'static tokio::runtime::Runtime);

impl Rt {
    fn new() -> Result<Self, String> {
        static GATEWAY_RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
        match GATEWAY_RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("lean-ctx-gateway")
                .enable_all()
                .build()
                .map_err(|e| format!("failed to start runtime for gateway: {e}"))
        }) {
            Ok(runtime) => Ok(Self(runtime)),
            Err(error) => Err(error.clone()),
        }
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.0.block_on(fut)
    }
}
/// Execute a `ctx_tools` action, returning response text or an error message.
///
/// `project_root` is the caller's project root (from `ToolContext`); it is
/// forwarded to [`mcp_catalog::proxy`] so output post-processing (#1095) can index
/// downstream results into the project's stores. Empty = no project scope.
pub fn run(args: &Map<String, Value>, project_root: &str) -> Result<String, String> {
    run_with_progress(
        args,
        project_root,
        crate::core::authority_mode::AuthorityMode::Legacy,
        None,
    )
}

/// Trusted scope view of the downstream config: only servers allowed by
/// [`server_allowed_in_scope`] survive. The shared catalog is never mutated —
/// this filtered config feeds only the calls below, so a rootless query cannot
/// change what bound sessions (or the dashboard) see.
fn scoped_gateway(
    cfg: &mcp_catalog::GatewayConfig,
    authority_mode: crate::core::authority_mode::AuthorityMode,
) -> mcp_catalog::GatewayConfig {
    let mut scoped = cfg.clone();
    scoped.servers = cfg
        .servers
        .iter()
        .filter(|server| mcp_catalog::server_allowed_in_scope(authority_mode, server))
        .cloned()
        .collect();
    scoped
}

/// Execute a ctx_tools action with optional UI-only downstream progress forwarding.
pub fn run_with_progress(
    args: &Map<String, Value>,
    project_root: &str,
    authority_mode: crate::core::authority_mode::AuthorityMode,
    progress: Option<mcp_catalog::client::ProgressCallback>,
) -> Result<String, String> {
    let cfg = Config::load();
    if !cfg.gateway.enabled_effective() {
        return Err(DISABLED_HINT.to_string());
    }
    // One trusted policy snapshot drives every action (list/find/refresh/call),
    // applied before ranking, transport creation or discovery. In rootless
    // scope only servers explicitly configured with `workspace_required=false`
    // are reachable — a handle, alias or cold call for anything else is denied
    // before a downstream process is spawned or a socket is opened.
    let gateway = scoped_gateway(&cfg.gateway, authority_mode);
    if gateway.active_servers().next().is_none() {
        if authority_mode.is_rootless() && cfg.gateway.active_servers().next().is_some() {
            return Err(
                "rootless (no-workspace) mode: every configured downstream server requires \
                 workspace authority. Restart with an explicit workspace root, or mark \
                 independently verified integrations with `workspace_required = false` in the \
                 trusted global gateway configuration."
                    .to_string(),
            );
        }
        return Err(
            "gateway is enabled but no downstream servers are configured \
             (add one or more [[gateway.servers]] entries)."
                .to_string(),
        );
    }

    // L4: expose any compression-integration addon as a named lean-ctx
    // compressor (once per process). No-op when none are configured.
    mcp_catalog::adapters::compression::ensure_registered(&cfg.gateway);

    let action = args.get("action").and_then(Value::as_str).unwrap_or("find");
    let rt = Rt::new()?;

    match action {
        "find" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let outcome = rt.block_on(mcp_catalog::find(&gateway, &query));
            Ok(mcp_catalog::render_cards(&outcome))
        }
        "list" => Ok(rt.block_on(mcp_catalog::servers_overview(&gateway))),
        "refresh" => {
            mcp_catalog::catalog::invalidate();
            let outcome = rt.block_on(mcp_catalog::find(&gateway, ""));
            Ok(format!(
                "gateway catalog refreshed.\n\n{}",
                mcp_catalog::render_cards(&outcome)
            ))
        }
        "call" => {
            let tool = args.get("tool").and_then(Value::as_str).ok_or_else(|| {
                "call requires 'tool' — a `server::tool` handle from `ctx_tools find`".to_string()
            })?;
            let arguments = match args.get("arguments") {
                Some(Value::Object(m)) => m.clone(),
                None | Some(Value::Null) => Map::new(),
                Some(_) => return Err("'arguments' must be a JSON object".to_string()),
            };
            // Pre-transport policy check, covering the missing-catalog-entry
            // case too: the owning server must exist in the scoped (allowed)
            // view before any downstream transport is created.
            if let Some((server_name, _)) = mcp_catalog::catalog::split_namespaced(tool)
                && !gateway
                    .active_servers()
                    .any(|server| server.name == server_name)
            {
                return Err(format!(
                    "downstream server `{server_name}` is unavailable in this scope"
                ));
            }
            rt.block_on(mcp_catalog::proxy_with_progress(
                &gateway,
                tool,
                arguments,
                project_root,
                progress,
            ))
        }
        other => Err(format!(
            "invalid action '{other}' (use: find, call, list, refresh)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// With the gateway disabled (default), every action returns the enable hint
    /// without touching the network. We assert this in a fresh runtime since
    /// `run` resolves a runtime handle.
    #[test]
    fn disabled_gateway_returns_hint() {
        let _env_lock = crate::core::data_dir::test_env_lock();
        // Ensure no env override flips it on.
        crate::test_env::remove_var("LEAN_CTX_GATEWAY");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let args = json!({ "action": "find", "query": "anything" });
        let out = rt
            .block_on(async { tokio::task::block_in_place(|| run(args.as_object().unwrap(), "")) });
        // Either disabled (no global config) — the dominant case in CI — or, if a
        // developer machine has it enabled, we still must not panic. We only
        // assert the message shape in the disabled/empty case.
        if let Err(e) = out {
            assert!(e.contains("gateway is disabled") || e.contains("no downstream"));
        }
    }

    /// CLI-Pfad-Regression (#ctx_tools): `run` wird OHNE ambienten Tokio-Runtime
    /// aufgerufen (wie `lean-ctx call ctx_tools …`). Der process-lifetime gateway
    /// runtime darf auch hier niemals paniken.
    #[test]
    fn run_without_ambient_runtime_does_not_panic() {
        let _env_lock = crate::core::data_dir::test_env_lock();
        crate::test_env::remove_var("LEAN_CTX_GATEWAY");
        let args = json!({ "action": "list" });
        let _ = run(args.as_object().unwrap(), ""); // Ok|Err, niemals Panic
    }
    #[test]
    // The spawned task must live on the shared process-lifetime runtime, so the
    // handle is awaited from a later `Rt` — the yield-then-await split is the
    // point of the test, not an accident (clippy::async_yields_async).
    #[allow(clippy::async_yields_async)]
    fn gateway_runtime_survives_between_tool_calls() {
        let first = Rt::new().expect("gateway runtime");
        let (tx, rx) = tokio::sync::oneshot::channel::<u8>();
        let task = first.block_on(async {
            tokio::spawn(async move { rx.await.expect("runtime stayed alive") })
        });
        // Rt wraps a &'static runtime and is not Drop; releasing the handle
        // documents that the task must survive the caller's handle going away.
        let _ = first;

        let second = Rt::new().expect("same gateway runtime");
        let value = second.block_on(async move {
            tx.send(7).expect("receiver still alive");
            task.await.expect("gateway task was not cancelled")
        });
        assert_eq!(value, 7);
    }

    /// Rootless scope: the trusted policy snapshot keeps only servers with the
    /// explicit config opt-out, while bound/legacy scope keeps everything.
    #[test]
    fn scoped_gateway_filters_by_authority_scope() {
        use crate::core::authority_mode::AuthorityMode;

        let needs_workspace = mcp_catalog::GatewayServer {
            name: "codegraph".into(),
            ..Default::default()
        };
        let workspace_free = mcp_catalog::GatewayServer {
            name: "gitlab".into(),
            workspace_required: false,
            ..Default::default()
        };
        let cfg = mcp_catalog::GatewayConfig {
            servers: vec![needs_workspace, workspace_free],
            ..Default::default()
        };

        let rootless = scoped_gateway(&cfg, AuthorityMode::None);
        assert_eq!(rootless.servers.len(), 1);
        assert_eq!(rootless.servers[0].name, "gitlab");

        let bound = scoped_gateway(&cfg, AuthorityMode::Workspace);
        assert_eq!(bound.servers.len(), 2);
        let legacy = scoped_gateway(&cfg, AuthorityMode::Legacy);
        assert_eq!(legacy.servers.len(), 2);
    }
}
