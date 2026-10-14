# Connection-scoped Workspace Registry, read-only phase

`ctx_workspace_get`, `ctx_workspace_list` and `ctx_workspace_diagnose` are advertised only in opt-in strict mode. The registry is owned by a `LeanCtxServer` instance, not keyed by a client-provided `threadId`, session id or workspace id. All three tools use the current server instance: `list` deliberately returns **only that connection's own workspace** and does not enumerate other connections. There is no arbitrary ID lookup.

An opaque random `workspaceId` is created per server instance. An in-memory revision starts at 0 and advances to 1 on the first materialized snapshot, incrementing on changes to canonical primary/additional roots. Repeated read API calls with unchanged roots preserve the ID and revision. The revision is real for the lifetime of this instance, **not persistent**: it resets on server restart and cannot serve as a global optimistic-write token yet.

`diagnose` reports the `read_only_untrusted_roots` state and explicit issues for unauthenticated workspace identity and unavailable mutations. This does **not** authenticate MCP client `roots/list`; server-validated Workspace bindings, durable registry and CAS mutations remain separate stages.

## Validation

`CARGO_BUILD_JOBS=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo build --bin lean-ctx -j 3` exit 0. All seven real-MCP bubblewrap scenarios passed. Strict-mode tests assert discovery, consistent stable workspace ID, revision 1, list cardinality 1, and diagnose status, alongside multi-root read/search enforcement and denied symlink escapes. Tests: `scripts/test_multiroot_search_mcp.py`.

## Next gate

Implement an authenticated and persisted Workspace service before exposing workspace selection or mutations. Add same-process multi-client isolation and revision/revocation tests. Do not use these ephemeral connection IDs as authorization credentials.
