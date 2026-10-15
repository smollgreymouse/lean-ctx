# Experimental persisted Workspace and live mutations (PR #7)

## Design

The launcher binds each LeanCtxServer to a `LEAN_CTX_WORKSPACE_FILE` path at construction, and uses local `lean-ctx workspace-admin` for management. No mutation credentials are passed to the MCP client. Neither a client-provided workspace ID nor MCP thread metadata chooses the backing store. In strict mode the persisted file supersedes untrusted `roots/list` and environment-wide allowed roots. The file contains UUID, persistent monotonic revision, canonical primary root and sorted additional roots.

`lean-ctx workspace-admin create STORE PRIMARY` initializes a Workspace file once; `workspace-admin add STORE ROOT EXPECTED_REVISION` and `remove` use revision-based CAS. Management is a separate local process controlled by the host OS file permissions, not an agent-visible MCP tool. Root normalization and broad-root rejection are checked. A stable locked sidecar serializes across processes, writes use temporary files, `fsync`, atomic replacement and parent-directory sync; ownership, mode 0600 and symlink checks protect state access. Files holding Workspace state cannot be placed within permitted roots.

New tool requests load the most recent state without restarting MCP; strict-mode process-global response caching is disabled. Tool dispatch uses request-scoped `WorkspaceSnapshot` and checks the persistent revision before publishing the result. This is a final-result fence, not an interrupt/cancellation mechanism for expensive already-running handlers.

To avoid bypasses, strict mode exposes only the audited `ctx_read`, `ctx_tree`, `ctx_search` readers and read-only Workspace inspection APIs. Other tools including shell, generic invoker, file resources, editing, and specialized readers are rejected or hidden; their admission policies must be independently audited before re-enabling them. Legacy mode is unaffected.

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

On 2026-10-15 the 9 bubblewrap MCP cases and 3-process shared-store test passed. A deterministic debug-only synchronization gate also exercises revocation after search execution and before response publication, and confirms the stale answer is denied. The latter confirms concurrent CAS with a single winner, live visibility of added/removed roots, and isolation from a separate launcher-bound Workspace.

## Unfinished gates

- Tests with **two MCP client sessions multiplexed inside one shared server process**; stdio MCP ordinarily uses a single connection per process. A concurrent thread-local PathJail unit test exists but must still be executed in the full Rust test harness.
- The deterministic in-flight response-publication race test passes, but cancellation of already-running expensive work and broader cache/index invalidation still require investigation. A restart-persistence test remains to be added.
- Lifecycle and backing-store trust hardening: choose a trusted launcher-owned directory, protect token transport, mitigate symlink/rename races of the parent directory, eliminate file ownership ambiguity on non-Unix platforms.
- Mutations are now a separate local CLI command, with file ownership as the OS privilege boundary. This is not a privileged daemon or remotely authenticated administrative API. Harden parent-directory and launcher identity before production use.
- Complete audits of lexical/vector index caches and other indirect response channels. Strict mode currently restricts tool availability and disables the global response cache, rather than proving every engine cache safe.

The debug-only `lean-ctx workspace-admin self-test-isolation` has been executed successfully and verifies two concurrent task-local authority contexts in one process; this does not simulate two full multiplexed MCP clients. `LEAN_CTX_STRICT_WORKSPACE=1` remains explicitly opt-in. Do not install this prototype into production Harr/LeanCTX or enable without completing the gates above.

## Separate administrative CLI and verified races (2026-10-15)

Management operations are **not registered with MCP**. Only the host-side CLI can create or mutate a Workspace file:

```text
lean-ctx workspace-admin create /absolute/state/workspace.json /absolute/primary
lean-ctx workspace-admin show /absolute/state/workspace.json
lean-ctx workspace-admin add /absolute/state/workspace.json /absolute/additional 1
lean-ctx workspace-admin remove /absolute/state/workspace.json /absolute/additional 2
```

MCP clients still have `ctx_workspace_get/list/diagnose` but do not receive the administration commands or any mutation token. Administrative authorization is based on access to the same-user local process and protected state directory; this is **not** a remotely authenticated management service.

Real MCP regression results: 9/9 bubblewrap scenarios PASS. Cross-process shared-store CAS and live revocation PASS. The debug-only test hook forces an in-flight search to pause immediately before response publication; root removal changes the revision and the response is rejected. `workspace-admin self-test-isolation` PASS exercises concurrent thread-local scopes in the same process, but full multiplexed MCP clients in a shared server remain unverified. Release binaries do not compile the debug-only race gate and self-test command.
