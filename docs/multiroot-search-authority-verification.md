# MCP multi-root search authority verification

Validated locally on 2026-10-14 against the standalone PR build (commit `06983da`), built using `cargo build --bin lean-ctx -j 3`, `CARGO_PROFILE_DEV_DEBUG=0`, and `CARGO_INCREMENTAL=0`. Build exit status: 0.

A real MCP JSON-RPC client launched the directly built LeanCTX ELF inside `bubblewrap`, with only synthetic repositories mounted, a fresh private HOME/XDG/data directory, and networking disabled. Nothing from the user's live projects, Codex or Harr configuration was mounted into the sandbox.

| Mode | ctx_tree / ctx_read in repo-b | ctx_search in repo-b | Search in foreign repo-c / symlink escaping root |
| --- | --- | --- | --- |
| No additional paths | denied | denied | denied |
| `LEAN_CTX_ALLOW_PATH=/fixture/repo-b` | allowed | denied by request authority | denied |
| `LEAN_CTX_EXTRA_ROOTS=/fixture/repo-b` | allowed | **1 BM25 result from repo-b**, correct text in `unique-b.txt` | denied |

The original underscore-heavy synthetic marker produced zero BM25 matches despite successful indexing; the final fixture uses ordinary tokens `cobalt lantern walnut` to verify that the result content actually comes back. All three modes passed with the executable assertions in `scripts/test_multiroot_search_mcp.py`.

Reproduce on Linux: `LEANCTX_TEST_BIN=/absolute/path/to/built/lean-ctx python3 scripts/test_multiroot_search_mcp.py extra` (repeat for `default` and `allow`). The script requires `bubblewrap` and does not use the production wrapper.

**Scope:** This verifies **explicit configured multi-root** search and PathJail behavior. It does *not* demonstrate live additions/removals in an already open Codex session, per-chat workspace identity, read-only root behavior, semantic/vector search, or multi-session cache revocation. Those are separate Workspace API requirements in Harr PR #20 and require their own tests before claiming complete dynamic multi-root support.
