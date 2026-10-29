# LeanCTX gateway: audit of intermittent downstream MCP connection drops

Read-only audit of `rust/src/` (2026-10-27), motivated by reports of
intermittent MCP connection drops through the leanctx gateway. Every finding
is either fixed in the working tree (✅) or explicitly deferred with a
reason. The downstream pool lives in each `lean-ctx mcp` stdio server
process (not in the HTTP daemon).

## Fixed

### ✅ 1. Pool liveness sweep could not detect a dead child (high)
`pool.rs` — `RunningService::is_closed()` never becomes true when a
downstream child crashes: the rmcp serve loop exits with `QuitReason::Closed`
without cancelling the token, and the stored handle stays `Some`. A crashed
session therefore stayed pooled, and every next tool call paid one failed
`Transport closed` before the evict-retry made it work again — the textbook
intermittent-drop signature. The sweep now also checks
`Peer::is_transport_closed()` (`tx.is_closed()`), which is true once the
serve task exits.

### ✅ 2. Evict-on-any-error killed healthy sessions on timeout (high)
`client.rs::proxy_call_with_progress` evicted the pooled session on **any**
error, including a plain request timeout. rmcp timeouts leave the transport
healthy (a cancel notification is sent downstream), so every slow call
(> `call_timeout_secs`, default 30 s) killed a live child — SIGKILL after the
3 s grace — and the next call paid a full respawn: spawn + `initialize`
handshake +, with a binary pin, a full SHA-256 of the pinned binary. For
long-running downstream tools this is a perpetual respawn loop that looks
exactly like intermittent drops. Eviction now happens only on broken
transports (`is_timeout_error()` distinguishes the two); timeouts keep the
pooled session alive.

### ✅ 3. Connect budget now has its own floor; pin verification is cached
`client.rs` bounded the whole connect (spawn + handshake +, with a pin, a
full binary hash) by the call timeout (30 s); cold `npx -y` starts exceed
that and die mid-handshake, every retry repeating the cost. Connects now get
`max(call_timeout, 120 s)` (`MIN_CONNECT_TIMEOUT`). `binary_pin::verify`
memoizes the digest per (mtime, size) with a 60 s TTL (`PIN_CACHE_TTL`),
so respawns stop re-hashing a 100 MB binary every time. A changed size or
mtime invalidates the entry immediately; otherwise the digest is reused for
at most 60 s from the last digest calculation (cache hits do not extend that
window). Regression tests:
`a_tampered_binary_fails_the_pin_again`, `timeouts_are_not_classified_as_broken_connections`.

### ✅ 4. Concurrent cold opens are coalesced per key
`pool::acquire` used to let N parallel first calls spawn N children (N−1
killed by the last-writer-wins insert). A per-key connect gate now
serializes cold opens: waiters re-check the pool after the gate and reuse
the winner's session.

### ✅ 5. Busy sessions are no longer expired as idle
`Entry.in_flight` counts calls holding a session; the idle-TTL sweep skips
sessions with calls in flight, and `pool::release()` (called by
`proxy_call_with_progress` and `fetch_tools` on every completion path)
refreshes `last_used`. A call running longer than `IDLE_TTL` (5 min) no
longer gets its session swept from under it.

### ✅ 6. Parent watchdog has a grace window; orphan sweep checks age
`spawn_parent_watchdog` exited whenever `getppid()` changed — a daemonizing
spawn wrapper (setsid / double-fork) that exits right after spawning looked
identical to a dead IDE, killing the server ~5 s after connect. A change
within `LEAN_CTX_MCP_PARENT_GRACE_SECS` (default 15 s, `0` restores the old
behavior) now means "spawn wrapper": the watchdog stands down and stdin EOF
governs liveness. The startup orphan sweep (`cleanup_orphan_mcp_processes`)
additionally reads `etimes` and skips `ppid=1` processes younger than 60 s,
so a wrapper-daemonized server in use is not killed by the next server
start; a fresh orphan dies on its own stdin EOF anyway.

### ✅ 7. `ctx_tools` is exempt from the generic 120 s tool watchdog
The gateway passthrough bounds every await with its own timeouts (connect
budget, call timeout), so a handler cannot hang past those. The generic
watchdog only abandoned healthy long calls while the downstream call kept
running behind the agent's back — feeding respawn churn into items 2–5.

### ✅ 8. Pooled children shut down cleanly with the server
`shutdown_runtime_bounded` and the parent-watchdog exit path now call
`pool::clear()` before exit: dropping each session cancels its transport
and closes the child's stdin (rmcp graceful path) instead of letting the
process death SIGKILL every pooled downstream server.

### ✅ Diagnostics (ported from `fix/gateway-lifecycle-diagnostics`)
The pool logs, at `warn`: transport-closed evictions, idle-TTL expiry (with
`idle_secs`), concurrent connection replacement, eviction after a failed
request, and pool clears on catalog invalidation; transport opens at
`debug`; failed tool calls at `error` with pool key, tool name and error.

## Checked and found sound

- **Framing misdetection (audit item 9a) was a false alarm**: the
  `windows(...).next()` check in `mcp_stdio.rs` is a *prefix* check — a JSON
  line cannot start with `content-length` (JSON text starts with `{ [ " t f
  n`), so the wedge scenario is unreachable. No change made.
- No mid-flight kills via pool races (callers clone their `Arc`; insert/
  evict/clear/sweep only drop the map's reference); the pool `Mutex` is never
  held across `.await` and poison is recovered; downstream progress demux is
  race-free and cannot head-of-line-block the serve loop; `tools/list`
  retries once on broken connections only, `tools/call` is never retried
  (correct for non-idempotent tools); malformed inbound lines are skipped,
  not fatal; the 32 MiB line cap is a deliberate OOM guard.

## Deferred (with reasons)

- **OAuth re-login flow for expired refresh tokens** — upon verification
  (audit item 9d was overstated): the token is attached **per request** via
  rmcp's `AuthClient` (`transport/common/auth/streamable_http_client.rs` calls
  `get_access_token()` at request-build time), and `get_access_token`
  **proactively refreshes** 30 s before expiry, writing the refreshed
  credentials back to the encrypted store. A pooled HTTP session therefore
  keeps refreshing while it lives. When the refresh token itself is dead, the
  request fails with `AuthorizationRequired`; our eviction policy then evicts
  the session and the next open surfaces the actionable error
  "`<server>` needs a browser login first: run `lean-ctx addon auth <server>`".
  What is missing is an automated re-login — a feature, and deliberately not
  attempted from inside an agent tool call (headless context); the current
  loud, guided failure is the correct behavior.
- **32 MiB max line** — deliberate OOM guard, documented behavior.


## Verification

- `cargo test --lib -j 1 mcp_catalog` → 75 passed (incl. the new timeout-
  classification and pin-cache tests), plus the orphan/watchdog tests in
  `cli::dispatch::server`.
- `cargo test --test main suite::mcp_pool_lifecycle` — proxy lifecycle
  (bind → health → stop → port released → restart) passed.
- Full `--lib` on this machine went from 61 pre-existing failures to 8:
  52 of them (49 `cloud_client::entitlement_cache` + 3 `learning_store::storage`)
  were umask-dependent test fixtures — `tempfile::TempDir::new()` and
  `std::fs::create_dir` inherit the umask, and under a user-private-groups
  umask of 002 (Ubuntu default) the directories/files land group-writable,
  which the modules' own strict safety checks (correctly) refuse. The
  fixtures now pin 0o700/0o644 explicitly. The remaining 8
  (`server::roots`, `lsp::editor_bridge`, `core::semantic::e2e`,
  `providers::provenance`, `agent_connector::capability`) depend on ambient
  HOME/editor state and flake run-to-run; they reproduce without any of
  these changes and are CI's domain.
