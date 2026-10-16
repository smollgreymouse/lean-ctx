# Workspace read-only MCP API (phase 2)

`ctx_workspace_get` is advertised only when `LEAN_CTX_STRICT_WORKSPACE=1`. It returns a JSON document with `schemaVersion`, `primaryRoot`, `additionalRoots`, `rootSource`, `revision`, `mutable`, `authenticatedWorkspace`, `legacyGlobalRootsInherited`.

The values are calculated directly from this MCP server connection's current `SessionState`. No user-provided session ID can select another client's state. The same roots/list resolution used by ordinary tools runs before the read. Additional roots come exclusively from the current peer's `roots/list`, not the global `LEAN_CTX_EXTRA_ROOTS` or `LEAN_CTX_ALLOW_PATH`.

`revision: 0` is deliberately a placeholder, not a live change counter. `authenticatedWorkspace: false` explicitly warns that MCP `roots/list` itself is not authorization; `mutable: false` means no mutation API is exposed yet. This is **not** the global workspace registry/list API. A session identity and authenticated Workspace Service are required before mutations or cross-client lookups.

## Verification

Compiled in the isolated project with `CARGO_BUILD_JOBS=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo build --bin lean-ctx -j 3`: exit code 0. All seven `bubblewrap` real-MCP cases passed (`default`, `allow`, `extra`, `strict-default`, `strict-allow`, `strict-extra`, `strict-roots`), including assertions that the new tool is advertised only in strict mode and exposes no global paths. With client roots/list, its response contains `repo-a` and `repo-b`. Without it, `additionalRoots` is empty. `scripts/test_multiroot_search_mcp.py` contains the assertions.

Before declaring the Workspace API complete, implement server-validated identity, list/diagnose semantics, revisioned mutations, live update/revocation and same-process concurrent-client tests.
