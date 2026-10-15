# Experimental persisted Workspace and live mutations (PR #7)

## Design

The launcher binds each LeanCtxServer to a `LEAN_CTX_WORKSPACE_FILE` path at construction, with an optional `LEAN_CTX_WORKSPACE_MUTATION_TOKEN`. Neither a client-provided workspace ID nor MCP thread metadata chooses the backing store. In strict mode the persisted file supersedes untrusted `roots/list` and environment-wide allowed roots. The file contains UUID, persistent monotonic revision, canonical primary root and sorted additional roots.

`ctx_workspace_create` initializes the state file when missing. `ctx_workspace_add_root` and `ctx_workspace_remove_root` require the separately configured token and `expectedRevision` (CAS); root normalization and broad-root rejection are checked. A stable locked sidecar serializes across processes, writes use temporary files, `fsync`, atomic replacement and parent-directory sync; ownership, mode 0600 and symlink checks protect state access. Files holding Workspace state cannot be placed within permitted roots.

New tool requests load the most recent state without restarting MCP; strict-mode process-global response caching is disabled. Tool dispatch uses request-scoped `WorkspaceSnapshot` and checks the persistent revision before publishing the result. This is a final-result fence, not an interrupt/cancellation mechanism for expensive already-running handlers.

To avoid bypasses, strict mode exposes only the audited `ctx_read`, `ctx_tree`, `ctx_search` readers and Workspace management APIs. Other tools including shell, generic invoker, file resources, editing, and specialized readers are rejected or hidden; their admission policies must be independently audited before re-enabling them. Legacy mode is unaffected.

## Reproduce

In the separate development checkout only:

```bash
cd rust
CARGO_BUILD_JOBS=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo build --bin lean-ctx -j 3
cd ..
for mode in default allow extra strict-default strict-allow strict-extra strict-roots strict-store strict-create; do
  LEANCTX_TEST_BIN="$PWD/rust/target/debug/lean-ctx" python3 scripts/test_multiroot_search_mcp.py "$mode"
done
LEANCTX_TEST_BIN="$PWD/rust/target/debug/lean-ctx" python3 scripts/test_workspace_store_concurrency.py
```

On 2026-10-15 the 9 bubblewrap MCP cases and 3-process shared-store test passed. The latter confirms concurrent CAS with a single winner, live visibility of added/removed roots, and isolation from a separate launcher-bound Workspace.

## Unfinished gates

- Tests with **two MCP client sessions multiplexed inside one shared server process**; stdio MCP ordinarily uses a single connection per process. A concurrent thread-local PathJail unit test exists but must still be executed in the full Rust test harness.
- Rigorous in-flight response revocation test that races a long-running read against a mutation, plus restart persistence test. Current tests validate sequential live revocation and cross-process CAS.
- Lifecycle and backing-store trust hardening: choose a trusted launcher-owned directory, protect token transport, mitigate symlink/rename races of the parent directory, eliminate file ownership ambiguity on non-Unix platforms.
- The mutation token travels as an MCP tool argument, which exposes it to the calling agent. **Do not use this API in an untrusted tool environment or enable by default.** Move privileged mutations to a separately authenticated administrative channel before production use.
- Complete audits of lexical/vector index caches and other indirect response channels. Strict mode currently restricts tool availability and disables the global response cache, rather than proving every engine cache safe.

`LEAN_CTX_STRICT_WORKSPACE=1` remains explicitly opt-in. Do not install this prototype into production Harr/LeanCTX or enable without completing the gates above.
