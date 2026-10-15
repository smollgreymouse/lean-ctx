# Multi-root isolated security gate execution — 2026-10-15

Baseline commit before test extensions: `ba76115`; tested isolated development binary from `d84767f` (`cargo build --bin lean-ctx -j 3` previously passed). Harness: `scripts/test_multiroot_search_mcp.py`, `scripts/test_workspace_store_concurrency.py`; diagnostics `lean-ctx workspace-admin self-test-isolation`. Fixtures use `bubblewrap --unshare-net`, isolated temporary roots and state. No production installation was changed.

**Do not interpret partial coverage as 19/19 PASS.** Definitions remain in `docs/workspace-isolated-security-test-design.md`.

| Gate | Result | Evidence / outstanding portion |
|---|---|---|
| I1 | NOT IMPLEMENTED | Separate complete MCP handlers via in-memory duplex in one process not built |
| I2 | PARTIAL PASS | Two concurrently running `WorkspaceSnapshot` thread scopes PASS; complete MCP dispatch, shared SessionMode and cancellation/panic variants untested |
| I3 | PARTIAL PASS | Disallowed path/symlink, workspace list only self and direct forbidden tools covered; spoofing all workspace ID/roots-list combinations in shared process untested |
| I4 | NOT APPLICABLE | Existing production stdio transport does not multiplex independent clients |
| R1 | PASS (publication fence) | Deterministic in-flight `ctx_search` response denied after external admin remove |
| R2 | PARTIAL PASS | `before_publication` barrier verified; earlier phases and actual worker cancellation untested |
| R3 | NOT IMPLEMENTED | Streaming/progress/partial outputs not instrumented |
| R4 | PARTIAL PASS | Remove/re-add ABA stale `expectedRevision` denied, monotonic revisions validated; in-flight ABA and failed atomic write not tested |
| R5 | PARTIAL PASS | New queries after revoke denied, primary remains readable in baseline, restart persisted ID/revision; broad all-entrypoint and roots-list resurrection matrix untested |
| C1 | PARTIAL PASS | Fresh allowed read then denied cross-workspace direct query; full ANN cache provenance not instrumented |
| C2 | PARTIAL PASS | Same primary/different extra roots, warmed repeated query denied in fourth MCP process; no in-process shared-cache harness |
| C3 | NOT IMPLEMENTED | Poisoned persistent BM25/ANN with shared XDG and restart not tested |
| C4 | NOT IMPLEMENTED | Revocation during index build not tested |
| C5 | PARTIAL PASS | `ctx_read/tree/search`, symlink escape, blocked shell/resources covered; all indirect paths not audited |
| C6 | NOT IMPLEMENTED | Concurrent warm-up, memory pressure and eviction matrix untested |
| S1 | PASS for MCP tool surface | Local CLI only; direct MCP admin methods absent and denied; mutation token removed from MCP path |
| S2 | PARTIAL PASS | 0600 rejection, symlinked store path rejection, CAS, store outside roots; symlinked-parent races/crash writes untested |
| S3 | NOT IMPLEMENTED | Cross-UID and permission sandbox variants not exercised |
| S4 | PARTIAL PASS | Concurrent CAS single winner, stale revision after restart and ABA tested; extended stress loop untested |

## Commands and observations

```text
rust/target/debug/lean-ctx workspace-admin self-test-isolation
  PASS: concurrent workspace request scopes are isolated
for mode in default allow extra strict-default strict-allow strict-extra strict-roots strict-store strict-create; do ...; done
  9/9 PASS
LEANCTX_TEST_BIN=... python3 scripts/test_workspace_store_concurrency.py
  PASS: warm-cache same-primary separate-authority rejection
  PASS: in-flight response suppressed after root revocation
  PASS: ABA denial, 0600 and symlink protections
  PASS: shared-store CAS, live add/remove, isolated other workspace, revision stability
```

**Security release gate is NOT MET.** Full in-process MCP duplex tests, streaming output revocation, index poisoning/ANN cache, index-build races and process/privilege hardening remain. Next priority: implement the real in-process dual-MCP handler harness (I1/I2) before claiming cross-client isolation.
