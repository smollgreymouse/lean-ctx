# Remaining multi-root security gates — architecture-specific test design

Status: design for the remaining release blockers after the isolated tests recorded on 2026-10-15. This document defines harnesses and expected invariants; it does **not** mark them passed. Applies to unified PR #7.

## Architecture findings that shape the tests

1. Strict lexical search (`ctx_search` local/BM25) builds a fresh `BM25Index` from original sources through `ctx_semantic_search::admitted::fresh_index`; authorization is checked before each source read. This is safer than trusting a persisted BM25 index, but races during source enumeration/read still need explicit revocation tests.
2. Dense/vector data uses `index_namespace::vectors_dir(project_root)`: namespace is based on project identity / primary root (optionally branch), **not Workspace ID or revision**. Two Workspaces with the same primary root but different attached roots can therefore share the same persisted vector namespace unless higher layers keep corpora separate. Treat this as a high-priority contamination test.
3. `ann_cache` is process-wide (`OnceLock<Mutex<Option<Cached>>>`) and keyed by an embedding-content fingerprint, not Workspace identity/revision. It may be safe when the exact corpus and alignment are identical, but this must be proven under authority changes, collisions/near-collisions, and concurrent corpus swaps.
4. Request cancellation already uses a request-local cancellation token. Workspace revocation currently fences final publication by revision; it does not necessarily interrupt expensive work.
5. MCP progress uses `LeanCtxServer.progress_sender: Arc<Mutex<Option<ProgressSender>>>`. That is a **server-wide mutable slot**, not request-local. If one `LeanCtxServer` ever serves concurrent clients/requests with different peers, a later request can overwrite the sender. Before claiming multiplexed-client safety, either prove serialization or move progress sender into request-scoped context.
6. `workspace_store_path` is currently bound on `LeanCtxServer`. Therefore one server instance cannot naturally represent different trusted Workspace bindings for multiple clients. Full multiplexing of mutually isolated Workspaces requires a connection-scoped authenticated binding, not merely more tests around the current field.

## Harness A — shared-process dense/BM25 contamination

Create `tests/workspace_security/cache_isolation.rs` as a debug/integration binary test with a 3-thread Tokio runtime and two independent `LeanCtxServer` instances in one process. Both use the **same primary repo P** and the same isolated `LEAN_CTX_DATA_DIR`, but different persisted Workspace stores:

- Workspace A: primary P + attached secret repo X.
- Workspace B: primary P only (or P + different attached repo Y).
- X contains marker `X_ONLY_VECTOR_7f91`; Y contains `Y_ONLY_VECTOR_c302`; P contains a deliberately similar decoy phrase.
- Use enough deterministic generated chunks (> `ANN_MIN_VECTORS`) to force the real HNSW cache path without production data. Embeddings must be deterministic/offline; if the normal engine is unavailable, add a test-only deterministic embedding provider behind `cfg(test)` / debug feature rather than skipping ANN.

### A1: fresh BM25 authority

A searches X successfully; B issues the identical query/path and must receive explicit authorization denial, not zero hits. Repeat with order B→A and concurrent A/B. Assert no marker/path/count from X appears in B response, logs, progress or diagnostics.

### A2: same-primary persisted dense namespace

Build dense index while A has X attached. Without deleting the shared XDG/data directory, start B with the same P but no X. B performs hybrid/dense search with the same query. PASS requires either:

- B cannot load/search corpus entries that are not admitted by B, or
- the implementation detects Workspace/revision mismatch and rebuilds/partitions the index.

Any X filename, marker, result count revealing X, or dense candidate derived from X is FAIL. Repeat after restart of B to exercise persisted vector files.

### A3: process-wide ANN swap

A warms HNSW with corpus A; B then warms a different corpus B in the same process; alternate 100 queries A/B with barriers around `ann_cache::topk`. Verify each returned ANN index is aligned only with the caller's embedding corpus. Instrument test-only cache events `{fingerprint,n_vectors,build/reuse}` without source text. Run A→B→A and B→A→B.

### A4: ANN fingerprint adversarial corpus

Construct deterministic corpora differing in vectors not sampled by the current fingerprint (first/middle/last component sampling). Attempt to create identical fingerprints with different full embeddings. If achievable, force HNSW path and prove whether reused graph can return indices invalid for the caller corpus. A collision yielding cross-corpus results is a security/correctness FAIL and should cause cache key to become cryptographic/full-content or include corpus identity. If collision construction is not practical, keep the test as a property/fuzz target rather than mark PASS.

### A5: revocation while fresh BM25 builds

Add debug barriers inside `fresh_index` at: `after_authority_check`, `before_source_read(relative)`, `after_source_read(relative)`, `before_index_return`. Start A search in X, revoke X through admin CLI, then release at each barrier. Final response must be denied by revision fence. Additionally, after revocation no *subsequent* source read from X should occur once cancellation/revision is observed. Record read counter and revision only.

### A6: revocation during dense build/alignment

Barriers around `load_engine_and_index`, `ensure_embeddings`, ANN build and `dense_backend::hybrid_results`. Revoke X at each stage. No X content may be published; newly persisted embeddings/index metadata must not become queryable by B after revocation. If expensive work continues privately, test bounded cleanup and ensure next request uses current revision.

## Harness B — streaming/progress egress

Build `tests/workspace_security/progress_isolation.rs` using real `rmcp::serve_directly` with two independent transports and explicit progress tokens. Capture every JSON-RPC message, not only final results.

### B1: per-request progress routing

Two concurrent requests on the **same cloned server state** use tokens `progress-A` and `progress-B`. Insert deterministic tool progress events with distinct messages. PASS requires every notification for A to go only to peer A/token A and every B notification to B. Given the current global `progress_sender` slot, this test is expected to expose a race if actual request overlap is possible. Do not weaken the test; fix by moving `ProgressSender` into request-local `ToolContext`/task-local scope.

### B2: revocation before progress containing sensitive data

No progress message should contain source snippets/paths from a root after its revocation revision commits. If tools only emit generic percentages/messages today, assert that as an invariant and add a regression preventing source-derived progress text. Revocation must prevent any future sensitive notification even if final response is fenced.

### B3: queued progress race

Block `ProgressSender` queue drain, enqueue A events, revoke A/root, then release. If queued messages can contain authority-sensitive data, they need revision tags and an egress check at send time. If messages are guaranteed nonsensitive, encode/test that schema contract. Final status: explicit policy, not accidental safety.

### B4: cancellation + progress cleanup

Cancel A while B remains active. A's sender/token must be removed; later B messages cannot route to A and no A notifications occur after cancellation acknowledgment. Panic/error path must also clear request-local sender.

## Harness C — long-lived multiplexed MCP clients

### Required architecture change before this can be a meaningful security test

Current `LeanCtxServer.workspace_store_path` is server-scoped. For mutually untrusted long-lived clients, introduce `ConnectionAuthority` (or equivalent) created by the trusted transport/launcher:

```text
ConnectionAuthority {
  connection_id: opaque server-generated id,
  workspace_store_path: trusted PathBuf,
  workspace_id: verified UUID,
  initial_revision: u64,
}
```

The client must never select `workspace_store_path` or gain authority from `roots/list`/thread IDs. Each incoming request receives its connection authority through `RequestContext` / task-local context. Read-only Workspace APIs return only that connection's binding. `WorkspaceSnapshot` is materialized from the bound store per request.

Only after this exists should C1-C5 be run. Do **not** add an HTTP endpoint merely to satisfy the test. Use a test-only multi-connection transport or the real production multiplexer once available.

### C1: two long-lived connections, one service process

Connection A bound to store A; connection B to store B. Keep both initialized simultaneously for hundreds of interleaved requests. Test `tools/list`, get/list/diagnose, read/tree/search, cancellation and progress. No root/ID/path disclosure across peers.

### C2: shared SessionMode collision

Construct both connections with the same logical channel/session identifiers where shared session pooling would normally reuse state. Their Workspace authority must remain connection-scoped even if non-security session/cache state is shared. If SessionState currently contains authorization-bearing roots, split those fields out before enabling shared multiplexing.

### C3: forged metadata

A copies B's workspace ID, thread ID, channel ID, root declarations and progress token. Authority must remain A's trusted connection binding. B's UUID should not be usable as a selector/capability.

### C4: disconnect/reconnect

Disconnect A during a blocked request; ensure cancellation and sender cleanup. Reconnect a new A connection to the same trusted store: it sees current persistent revision but receives a fresh connection ID. No old task-local/request state survives.

### C5: connection churn/fuzz

100 bounded randomized schedules with 2–8 connections, independent stores, add/remove mutations via host admin, cancellations and reconnects. Every response/notification tagged in the harness with expected `(connection,workspace,revision)`; zero cross-authority markers permitted.

## Harness D — OS user / persistent store privilege boundary

Run only inside disposable user namespaces/containers; never create real system users on the workstation.

### D1: ownership

Create store as UID A mode 0600. Run `workspace-admin show/add/remove` and MCP as UID B. All must fail before JSON parsing/content access. UID A succeeds. Repeat for lock file and temporary replacement file.

### D2: symlinked parent / rename race

Attacker controls a parent component and swaps it/symlinks it between validation and open/rename. Use a deterministic helper process and barriers around state open and atomic rename. PASS requires operations remain anchored to a trusted directory/file descriptor (`openat2`/dirfd strategy on Linux) or fail closed. Current path-based validation should be considered **not proven** against this race.

### D3: interrupted writer

Kill admin process at test-only checkpoints: after temp create, after write, after temp fsync, after rename, before parent fsync. On restart, store must be either old valid state or new valid state, never truncated/corrupt authority. Orphan temp files must not be interpreted as authority.

### D4: permission matrix

Reject store/lock files with group/world bits, non-regular files, relative paths, store inside allowed root, broad primary root, root symlink aliases, foreign owner. Verify error messages do not disclose file content.

## Instrumentation rules

Add a single debug-only `workspace_test_hooks` module rather than scattered environment-variable sleeps. Hooks are named barriers with bounded channels and compile out under `not(debug_assertions)` (prefer an explicit `workspace-security-test-hooks` Cargo feature for CI). Required hook points:

- request: `after_snapshot`, `before_publication`;
- BM25: `after_authority_check`, `before_source_read`, `after_source_read`, `before_index_return`;
- dense: `before_index_load`, `before_embed`, `after_embed`, `before_ann_topk`, `after_ann_topk`;
- progress: `before_enqueue`, `before_send`;
- store: `before_open`, `before_rename`, `after_rename_before_dir_fsync`.

Controller releases barriers explicitly; no correctness assertion may depend on `sleep`. Each event records only request ID, connection ID, workspace UUID, revision, stage and hashed path/corpus ID.

## PASS/FAIL policy and execution order

1. **First B1**: progress routing is an obvious shared-state hazard and may require refactor before true multiplexing.
2. **A2/A3/A5/A6**: same-primary dense namespace, global ANN, and build-time revocation.
3. **D1-D4**: persistent state OS boundary and crash/race durability.
4. Implement `ConnectionAuthority`, then C1-C5 and shared SessionMode tests.
5. Re-run all existing 9 bubblewrap modes, cross-process CAS/ABA, in-flight publication fence, same-process dual handler test, plus new suites with `-j 3`.

A release candidate is acceptable only when every applicable case reports PASS; unsupported production transport cases may be `NOT APPLICABLE`, never silently promoted to PASS. Any test that cannot force the intended cache/index/progress path is `NOT EXECUTED` and must print why (e.g. embeddings feature unavailable, ANN threshold not reached).
