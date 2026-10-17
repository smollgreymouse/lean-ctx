# Isolated multi-root security acceptance tests — design

Status: **test design only**, not an assertion that these tests pass. Applies to unified PR #7. Production installation must not be used as a test fixture. Build with `-j 3` maximum; run only one memory-heavy Cargo build at a time.

## Threat model and invariant

A and B are mutually untrusted clients, each authorized by the trusted launcher to different workspace files. `roots/list`, user-supplied IDs, `threadId`, config env extras, MCP resource URIs, explicit `path`, symlinks, search indices and caches **must not** confer additional authority. Removing a root at revision r+1 must prevent any response containing content from that root from being published after the revocation commit. Do not confuse "no search hits" with an authorization denial.

All tests run in an unprivileged `bubblewrap --unshare-net` fixture with temporary HOME/XDG/state/cache paths, unique markers (e.g. `orion_cobalt_A_secret`, `violet_amber_B_secret`), deterministic artificial barriers and no production mount except a read-only copy of the test binary. Assert exact response codes/content and evidence of handler execution. No network, shell escape, user home, credentials or real repos. Logs must not contain forbidden markers or management secrets. Parent/child processes must terminate and remove fixtures on timeouts. Fresh fixture per test, fixed seeds and no wall-clock sleeps as correctness predicates.

## Fixture and harness contract

- `repo-a`, `repo-b`, `repo-c`, `outside` each contain distinguishable source text, a `.git` directory, symlinks and nested paths. Include misleading near-match content to force BM25/ANN indexing and cache access; ensure a valid positive result exists before testing denial.
- `store-A` grants `repo-a` plus `repo-b`, `store-B` grants `repo-c`, `outside` is always disallowed. Stores are `0600` outside all roots, unique UUIDs and revision 1. Read-only MCP clients cannot invoke management; admin CLI writes stores through a separate controlled process.
- Instrument *test-only*, feature/cfg-gated barriers: `after_snapshot`, `before_index_hit`, `after_index_hit`, `before_publication`, `after_revocation_commit`. Each waits for a one-shot explicit release signal (IPC channel/Unix socket); never use unbounded sleeps or expose hooks in release binaries. Test logs include workspace ID, request ID, revision and stage, but no source data.
- A controller runs the MCP clients concurrently, drives barriers and admin CAS mutations, collects result JSON and audit events, and enforces individual RPC and global fixture timeouts. `PASS` requires both positive control and denied negative control.
- Run cold and warm variants. Repeat randomized interleavings (at least 100 schedules, bounded) and a deterministic minimal reproducer for any failure. Keep CI smoke subset small, with full randomized suite as separate security gate.

## Gate I: actual in-process multi-client separation

`I1`: Two separate `LeanCtxServer` instances in the *same Rust process*, each with its own connection context and trusted launcher-bound workspace file. Use two independent in-memory duplex streams speaking real MCP JSON-RPC, not just direct `PathJail` invocations. Concurrent initialize -> tools/list -> ctx_workspace_get -> ctx_read/ctx_tree/ctx_search. A may retrieve A and B; B can retrieve C, never A or B. Assert opaque ID, `additionalRoots`, revision and audit correlation remain distinct.

`I2`: Synchronize A and B across `after_snapshot` and `before_index_hit` while swapping completion order. Trigger both direct calls and the real `spawn_blocking` path. Assert no task-local `REQUEST_WORKSPACE`, `REQUEST_PROJECT`, `REQUEST_SEARCH_ROOTS` leakage after handler completion, error, cancellation or panic. Test both personal and shared `SessionMode` (shared session cache is especially important); reuse of `SessionState` must not make peers share authorization.

`I3`: A sends forged workspace IDs, MCP roots/list claims, resource URI and path pointing at C; B attempts A's paths. Always deny. Cross-client `tools/list` and read-only `workspace.list` must not disclose other Workspace identities or absolute root paths.

`I4`: If production eventually multiplexes MCP transports through a single server object (Streamable HTTP/etc.), separately run two *real connections* against that transport. Until a supported multi-connection transport exists, label this `NOT APPLICABLE / NOT PROVEN` rather than claiming `PASS` based on stdio or in-memory objects. Do not add HTTP only to satisfy a test.

## Gate R: in-flight cancellation and revocation semantics

`R1`: Positive control with B granted. Start an expensive B search/read and block before publication. Admin removes B (CAS rev 1 -> 2) and commits; release response. Assert fail-closed error and **zero B marker bytes** in response, error, logs, notifications, streaming chunks or tool calls. This tests the existing publication fence.

`R2`: Block at `after_snapshot`, `before_index_hit`, `after_index_hit` and `before_publication` in separate runs. Revoke, then release. Verify no unauthorized content egress; assert a revoked task either cancels with explicit reason, or completes privately but discards output. Measure bounded cancellation/cleanup and no stale reference retained across a subsequent request. If an actual cancellation guarantee is required, add a revision subscription/cancellation token and test it at every costly work checkpoint (not simply final-response blocking).

`R3`: Repeat R1/R2 for each supported streaming/progress/partial-output path. If a chunk can be emitted before final response, the final-only revision fence is insufficient: require a per-chunk authority recheck or prohibit streaming sensitive content while an operation can be revoked.

`R4`: Remove B and re-add B with revisions 2 -> 3 while an old rev-1 request is waiting. The old request **must not** be authorized solely because its path is currently allowed (ABA defense); require exact revision match or new request snapshot. Also test invalid store, primary-root mismatch, deletion/rename of store, failed atomic write, CAS conflict and concurrent removals.

`R5`: New requests after revocation must deny B at all entry points; requests to still-authorized A must continue to work. After server restart, persistent store must retain revision and authorized roots; untrusted client `roots/list` cannot resurrect B.

## Gate C: cache/index contamination

Cache inventory to examine and prove with code-level tracing: `global_response_cache`, per-session `SessionCache`, `shared_sessions` pooling, BM25 graph scan reuse, `ctx_semantic_search::admitted::fresh_index`, ANN/vector caches (`core/ann_cache.rs`), persisted search databases, path/realpath caches, read stubs, summaries and any background index warming.

`C1` cold start: A indexes B, B then searches for A/B markers. Denial must be explicit; verify cache hit/miss/reuse events identify Workspace ID **and revision**, or strict mode bypasses affected cache completely.

`C2` warm cache: A searches B repeatedly, then the same query through other Workspace/server instance or after removing B. Check no hits, snippets, counts, filenames or inferred existence escape. Repeat with disjoint primary roots but identical search query, and with **same primary root but different additional roots** (critical cache-key collision case).

`C3` reuse poisoned state: prewarm BM25/ANN and shared-session caches under A; start B within the same process, then restart server with the same XDG/cache directory and B's store. Results must match a fresh cold B run, not A's warm data.

`C4` index build vs revocation race: pause while B is being indexed, revoke B, complete build, search again. B content must not be returned or retained in an authority-independent queryable index. Build success is not authorization to publish.

`C5` indirect paths: `ctx_read`, `ctx_tree`, `ctx_search`, resource handlers, tool wrappers, aliases, error diagnostics, follow-up expand/offset requests, symlink/hardlink aliases and case/path-normalization variants. Unknown or non-audited tools in strict mode must be absent from discovery **and** rejected on direct invocation, not merely hidden.

`C6` test same path under different root authorities and revisions with concurrent searches, cache eviction, memory pressure and background warm-up. Assert workspace provenance is checked at every data-egress boundary, not inferred from a file's physical path.

## Gate S: state and privilege hardening

`S1` admin mutations absent from MCP tools/list and rejected by direct tool call (including forged names and generic tool dispatch); no mutation secret in MCP argument schema, session content, stdout/stderr.

`S2` only a launcher-controlled store path is effective; tools cannot select, replace or move the Workspace file. Assert 0600 owner checks, symlink and symlinked parent attacks, replacement race, relative/broad root rejection, temporary-file rollback, interrupted writer recovery, lock file ownership, and crash durability (`fsync` + atomic rename).

`S3` file owner vs process user tests under different UIDs/containers, management CLI privilege boundaries and denied write to out-of-scope directory. Ensure the admin interface is not implicitly remote-exposed.

`S4` stress repeated CAS conflict and reset/restart. A stale expectedRevision must not win even when state is reopened; UUID must not silently change except creation of a distinct Workspace.

## Execution order, implementation deliverables and merge gates

1. Build `tests/workspace_security/` harness + deterministic barriers; implement I1/I2/I3 first. Do not rely on a pseudo-test that only creates threads: invoke the actual MCP handler over two duplex transports within one process.
2. Implement R2/R3/R4, adjust runtime cancellation/egress policy only as evidence requires. Keep the existing R1 passing.
3. Trace and test C1-C6; fix cache scoping, purge-on-revocation or disable strict-mode cache paths. Treat source indexing as an egress-adjacent operation.
4. Harden S1-S4 and add restart, crash and recovery fixtures. Finally run compatibility suite with `LEAN_CTX_STRICT_WORKSPACE` unset.

Required reports per scenario: fixture revision transition, request/workspace IDs, entry-point/tool, blocked stage, expected authorization, actual JSON result, audit trace, timeout/cleanup status. CI reports separate `PASS`, `FAIL`, `NOT IMPLEMENTED` and `NOT APPLICABLE`. No mark-as-pass for an unexecuted test. Primary security acceptance gate: zero cross-Workspace content and zero post-revocation egress, even under warm caches and adverse interleavings. Full build and test invocations use Cargo `-j 3` and isolated target/cache to avoid interfering with the installed LeanCTX.
