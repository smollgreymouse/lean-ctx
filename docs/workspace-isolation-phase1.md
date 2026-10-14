# Workspace isolation — phase 1 (opt-in)

Based on the multi-root search baseline from PR #4. An immutable `WorkspaceSnapshot` holds canonical `primary_root`, `additional_roots`, and reserved `revision`. The experimental `LEAN_CTX_STRICT_WORKSPACE=1` binds a snapshot from the current MCP session before tool dispatch. The request-scoped snapshot is carried into the `spawn_blocking` handler. PathJail then uses only its snapshot's additional roots; it does not use process-wide `LEAN_CTX_ALLOW_PATH`, `LEAN_CTX_EXTRA_ROOTS`, read-only roots or the state-directory exception. A disabled global path jail cannot bypass strict mode. The default legacy behavior is not changed.

## Verified real MCP behavior

Build: `CARGO_BUILD_JOBS=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo build --bin lean-ctx -j 3` — exit 0. Six hermetic bubblewrap MCP runs pass: `default`, `allow`, `extra`, `strict-default`, `strict-allow`, `strict-extra`. In strict-allow, ctx_read and ctx_search are both blocked outside the primary root. In strict-extra, BM25 search returns actual source content from the attached root. A foreign sibling and a symlink escape are denied in all modes. Tests are in `scripts/test_multiroot_search_mcp.py`.

## Remaining security gates (must pass before enabling by default)

- **Same-process concurrent clients**: prove independent session binding with two *simultaneous* clients sharing a server (this test currently uses separate MCP processes).
- Unit `strict_workspace_snapshots_isolate_clients_and_reject_cross_root_reads` was added, but compiling the complete Rust test harness exhausted available swap; its execution is **not verified**. It must pass on a constrained build runner.
- Authenticated Workspace identity; never trust raw client `threadId`/metadata as authorization.
- Session revocation/revision semantics and authorization recheck at publication boundaries.
- Audit direct source paths bypassing PathJail, protected resource paths, and cache/semantic/vector retrieval.
- Strict snapshot construction must fail closed when the session has no trusted primary root. Revision is currently a placeholder (`0`).

Do **not** mark the broader Workspace API or dynamic live-update isolation complete from this phase.
