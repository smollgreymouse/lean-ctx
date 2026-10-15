# Security acceptance execution follow-up (2026-10-15)

Unified PR #7; local uninstalled development build only. Build command: `CARGO_BUILD_JOBS=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo build --bin lean-ctx -j 3`; result **PASS**. No real projects or production stores modified.

## New in-process MCP handler verification

Debug-only `LEAN_CTX_STRICT_WORKSPACE=1 lean-ctx workspace-admin self-test-mcp-dual`: **PASS**. In one process, constructs independent `LeanCtxServer` instances with different trusted persisted Workspace files (A grants repo-a + repo-b, C grants only repo-c), and uses concurrent `ContextEngine.call_tool_value` calls, each internally dispatched over an independent `rmcp::OneshotTransport`. `ctx_workspace_get` reports the expected disjoint effective roots, concurrent `ctx_read` permits B for A but never discloses B's unique marker to C. Assertions test the actual server handler, not merely the PathJail helper. The CLI selftest is debug-only.

**Coverage limit:** `ContextEngine` issues separate one-shot JSON-RPC transports per request, not long-lived client sessions. There is no shared production TCP/HTTP MCP multiplexer here. Shared `SessionMode`, cancellation/panic behavior and `ctx_tree`/`ctx_search` under this same-process pair remain untested. Therefore I1 is **PARTIAL PASS**, not a complete security acceptance signoff, and I2 remains partial.

## Complete rerun

- Concurrent request-scoped PathJail thread test: **PASS**.
- 9/9 network-disabled bubblewrap MCP modes: **PASS** (`default`, `allow`, `extra`, `strict-default`, `strict-allow`, `strict-extra`, `strict-roots`, `strict-store`, `strict-create`).
- Cross-process live CAS, two peer MCP processes plus isolated third/fourth stores: **PASS**.
- Warm same-primary/different-additional authorization denial: **PASS**.
- In-flight response publication revocation: **PASS**.
- ABA CAS, state-file 0600 and symlink rejection: **PASS**.

## Still not validated

- **Index contamination:** cold/warm BM25 persisted-poisoning and ANN/vector-cache reuse across independently authorized clients in one process, with shared XDG state and restart (C1/C3/C4/C6). Existing warm-cache check covers only an authorized-vs-denied direct search, not poisoned ANN internals.
- **Index build race:** revoke during indexing, not only at final result publication (C4).
- **Streaming egress:** a complete streamed/progress chunk transport with per-chunk authorization during revocation (R3).
- **Multi-user:** different OS UIDs, parent symlink race, interrupted writer recovery, ownership/permission matrices (S2/S3).
- **Full long-lived MCP multiplexing:** distinct client sessions connected to a shared transport/service instance with session isolation and shared SessionMode (I1/I2/I4).

No broad claim of all 19 gates passing is justified. These items remain release blockers; do not enable strict mode by default.
