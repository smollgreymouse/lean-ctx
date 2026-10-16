# Workspace multi-root security acceptance — 2026-10-16

Scope: unified PR #7, isolated development checkout only. Production Harr/LeanCTX installation was not replaced. All builds used at most three Cargo jobs.

## Final acceptance commands

- `CARGO_BUILD_JOBS=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo build --bin lean-ctx -j 3` — **PASS**.
- `CARGO_BUILD_JOBS=3 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo check --tests -j 3` — **PASS**, clean final run.
- Debug security self-tests — **PASS**:
  - request-local progress routing + post-overlap cleanup;
  - shared `SessionState` with distinct trusted Workspace stores;
  - BM25 mid-build revocation + stale response suppression;
  - persisted dense namespace scoped by Workspace UUID **and revision**, including real `EmbeddingIndex::save/load` storage path;
  - adversarial ANN corpus cache swap using a full BLAKE3 corpus key;
  - concurrent MCP handlers in one process;
  - two long-lived duplex MCP sessions in one process (50 interleavings);
  - concurrent request-scoped PathJail authority.
- Real stdio MCP bubblewrap modes — **10/10 PASS**: `default`, `allow`, `extra`, `strict-default`, `strict-allow`, `strict-extra`, `strict-roots`, `strict-store`, `strict-store-forged`, `strict-create`.
- Shared-store concurrency suite — **PASS**: warm same-primary/different-authority denial, publication-time revoke fence, ABA denial, concurrent CAS single winner, restart persistence, 100-step deterministic CAS/live-reload churn.
- Persistent-store crash/race suite — **PASS**: killed before rename preserves old state; killed after atomic rename leaves valid monotonic state; parent-directory symlink/rename swap cannot redirect state after dirfd anchoring.
- Disposable Docker multi-UID suite — **PASS**: owner UID can use state; foreign UID denied in admin CLI and strict MCP. Fixture ownership is restored and temporary directories removed afterwards.
- Real Cursor/Windsurf hook files were checksummed before/after final self-tests and were **unchanged**. An earlier test-run side effect was restored from the installer's `.bak` files before final acceptance; self-tests now force headless startup.

## Security fixes driven by the tests

1. **Progress isolation:** remove server-global mutable progress sender; progress sender is request/task-local and dispatch obtains it from request scope. Overlapping requests with tokens A/B never cross-route, and a subsequent C request sees no stale sender.
2. **ANN cache:** replace sampled FNV fingerprint with a BLAKE3 hash over the complete embedding matrix. The process-global HNSW cache rebuilds for adversarially different corpora rather than reusing another corpus graph.
3. **Dense persistence:** `index_namespace` now includes strict Workspace UUID and revision. Legacy root-only vector migration is disabled while Workspace authority is active. Same-primary Workspaces and new revisions use different persisted vector directories; real `EmbeddingIndex` save/load cannot cross those namespaces.
4. **Workspace store filesystem boundary (Unix):** operations anchor the canonical parent directory using a dirfd; store/lock/temp access uses `openat(..., O_NOFOLLOW)`, replacement uses `renameat`, directory fsync happens through the opened directory descriptor. Parent path replacement cannot redirect an in-flight operation.
5. **Headless tests:** debug security self-tests do not execute startup rule/hook maintenance against the developer's real HOME.
6. **Authority provenance:** launcher-bound store wins over adversarial client `roots/list`, `workspaceId` and `threadId` metadata. Client declarations cannot expand strict authority.

## Gate matrix from the design

| Gate | Status | Evidence / boundary |
|---|---|---|
| A1 fresh BM25 authority | PASS | allowed positive control + forbidden real MCP queries, forged roots/meta denied |
| A2 same-primary dense persistence | PASS | different Workspace IDs/revisions produce separate directories; actual `EmbeddingIndex` save/load isolation verified |
| A3 process-wide ANN swap | PASS | adversarial A→B cache reuse test rebuilds and agrees with B brute-force top result |
| A4 sampled-fingerprint adversary | PASS for designed attack class | sampled key removed entirely; full BLAKE3 corpus key used |
| A5 BM25 build revocation | PASS for exercised mid-build barrier | revoke while blocked before source read; final stale output suppressed by revision fence |
| A6 dense build revocation | PASS for persistent-authority separation; worker cancellation not claimed | old revision writes are namespace-isolated; publication fence blocks old revision result; actual model computation is not forcibly cancelled |
| B1 concurrent progress routing | PASS | two overlapping real MCP handler calls route tokens independently |
| B2 sensitive progress after revoke | N/A in strict surface | strict Workspace exposes `ctx_read/tree/search`; sensitive semantic/reindex progress tool is not exposed |
| B3 queued sensitive progress | N/A under current strict surface | no authority-sensitive streaming/progress payload is exposed by allowed strict tools |
| B4 progress scope cleanup | PASS for completion/error scope; cancellation cancellation-token path separately exists | third request after overlap receives only token C; sender is task-local |
| C1 long-lived connections | PASS for two in-process long-lived duplex MCP services | 50 concurrent/interleaved iterations, separate stores |
| C2 shared SessionMode | PASS | deliberately shared `SessionState`, separate store authority, no extra-root leak |
| C3 forged metadata | PASS | real stdio MCP `roots/list` plus forged workspace/thread metadata cannot override launcher-bound store |
| C4 restart/reconnect | PASS for current process-per-session model | persistent UUID/revision reloaded; revoked paths remain denied |
| C5 churn/fuzz | PASS for bounded deterministic test | 100 CAS/live-reload operations plus stale-revision attempts observed by already-open clients |
| D1 multi-UID | PASS | disposable Docker UID 2001 vs 2002, no network |
| D2 parent symlink/rename race | PASS on Linux/Unix implementation | dirfd anchored before path swap; evil directory remains untouched |
| D3 interrupted writer | PASS at the security-critical atomic boundary | before rename => old valid state; after rename/before dir fsync => parseable monotonic new state |
| D4 permissions/path matrix | PASS for exercised cases | 0600 ownership, foreign UID, store symlink, store outside roots, unsafe/broad root validation |

## Explicit architecture boundary — next stage, not silently counted as PASS

The production Streamable HTTP service factory creates a fresh `LeanCtxServer` per MCP session, but `workspace_store_path` is still a server/launcher-bound value. There is not yet a transport-authenticated mapping that can bind **different** Workspace stores to different clients arriving through one shared HTTP listener. The tests prove that separate server instances with separate trusted bindings remain isolated, including when they share `SessionState`; they do not invent an untrusted client selector to emulate a missing production contract.

If Harr requires one HTTP/multiplexed listener to host mutually untrusted clients with different Workspace stores, the next implementation stage is `ConnectionAuthority` (trusted connection identity -> store binding), followed by HTTP session tests. Until that contract exists, client-supplied workspace/thread/root metadata must continue to be ignored for authority.

## Acceptance conclusion

The multi-root strict Workspace implementation passes the applicable isolation, revocation, cache, progress, persistence, crash and OS-owner security tests for its current launcher-bound architecture. Strict mode remains opt-in. The remaining `ConnectionAuthority` capability is a separate architecture extension, not an unexecuted test of the current contract.
