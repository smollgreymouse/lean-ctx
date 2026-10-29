//! Persistent downstream MCP session pool (#1078).
//!
//! Without pooling, every `ctx_tools` list/call reopened a connection — spawning
//! a fresh child process and re-running the MCP `initialize` handshake — so each
//! tool call paid the full spawn+handshake latency. This pool keeps one live
//! [`ClientService`] per distinct wiring and reuses it across calls:
//!
//! - **keyed** by the resolved transport (same command/args/env/caps/url → same
//!   session), so two different servers never share a child,
//! - **coalesced**: concurrent cold opens of the same key share one child via a
//!   per-key connect gate instead of racing spawns,
//! - **idle-evicted**: a session unused for `IDLE_TTL` is dropped on the next
//!   access (closing the child's stdin → the server exits), swept opportunistically
//!   on every [`acquire`]; sessions with calls in flight are never expired,
//! - **liveness-checked**: [`acquire`] also drops any session whose transport has
//!   closed (the child exited/crashed) *before* handing one out, so a request is
//!   never sent into a dead pipe — and callers never have to blindly re-send a
//!   request to recover (which could double-execute a non-idempotent tool).
//!
//! The map lock is a `std::sync::Mutex` held only for short, await-free critical
//! sections (the slow `open()` runs outside the lock), which keeps [`clear`]
//! callable from the synchronous config/catalog paths.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use super::client::{self, ClientService};
use super::config::ResolvedTransport;

/// Drop a pooled session after this long without use, so an idle child process
/// does not linger. The next call for that wiring transparently reopens.
const IDLE_TTL: Duration = Duration::from_mins(5);

struct Entry {
    service: Arc<ClientService>,
    last_used: Instant,
    /// Calls currently holding this session. Idle expiry is skipped while
    /// this is above zero: `last_used` is only refreshed when a call is
    /// handed out, so a long-running call would otherwise look idle to the
    /// sweep and its session would be gone by the time it finishes.
    in_flight: Arc<AtomicU32>,
}

fn pool() -> &'static Mutex<HashMap<u64, Entry>> {
    static POOL: OnceLock<Mutex<HashMap<u64, Entry>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-key serialization of cold opens. Two concurrent first calls used to
/// each run `open()` — spawn + handshake — and the last-writer-wins insert
/// killed the loser's child afterwards; with the gate the loser instead waits
/// for the winner and reuses its session.
fn connect_gate(key: u64) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<u64, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let gates = GATES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = gates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = guard.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    guard.insert(key, Arc::downgrade(&gate));
    gate
}

/// Stable identity for a resolved transport: same wiring -> same key -> same
/// pooled session. Secret values are replaced by memento fingerprints.
#[must_use]
pub fn key(transport: &ResolvedTransport) -> u64 {
    let mut h = DefaultHasher::new();
    pool_identity(transport).hash(&mut h);
    h.finish()
}

#[must_use]
fn pool_identity(transport: &ResolvedTransport) -> String {
    match transport {
        ResolvedTransport::Stdio {
            command,
            args,
            env,
            binary_sha256,
        } => format!(
            "stdio|command={command:?}|args={args:?}|env={env:?}|binary_sha256={binary_sha256:?}"
        ),
        ResolvedTransport::Http {
            url,
            headers,
            secret_fingerprints,
            oauth,
        } => format!(
            "http|url={url:?}|headers={:?}|secrets={:?}|oauth={oauth}",
            public_values_case_insensitive(headers, secret_fingerprints),
            normalized_secret_fingerprints(secret_fingerprints)
        ),
    }
}

fn public_values_case_insensitive(
    values: &BTreeMap<String, String>,
    secret_fingerprints: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    values
        .iter()
        .filter(|(name, _)| {
            !secret_fingerprints
                .keys()
                .any(|secret| secret.eq_ignore_ascii_case(name))
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn normalized_secret_fingerprints(
    secret_fingerprints: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    secret_fingerprints
        .iter()
        .map(|(name, fingerprint)| (name.to_ascii_lowercase(), fingerprint.clone()))
        .collect()
}

/// A live session for `transport`, reusing a pooled one when present + fresh, or
/// opening (and caching) a new one. Sweeps idle sessions on the way in.
pub async fn acquire(
    transport: &ResolvedTransport,
    timeout: Duration,
) -> Result<AcquiredSession, String> {
    let k = key(transport);
    {
        let mut map = lock();
        let now = Instant::now();
        // Sweep on the way in: drop sessions that are idle-expired *or* whose
        // transport has closed (child exited/crashed). Dropping an Entry releases
        // its `ClientService`, which tears down the child. A dead session is thus
        // never handed out, so callers never send into a dead pipe.
        map.retain(|entry_key, e| {
            let idle = now.duration_since(e.last_used);
            // `is_closed()` alone cannot see a crashed downstream child: the
            // serve loop exits with QuitReason::Closed without cancelling the
            // token, and the request channel closing is the only reliable
            // signal that the transport is gone.
            let closed = e.service.is_closed() || e.service.is_transport_closed();
            if closed {
                tracing::warn!(
                    pool_key = entry_key,
                    "downstream MCP transport closed; evicting session"
                );
            } else if e.in_flight.load(Ordering::Relaxed) == 0 && idle >= IDLE_TTL {
                tracing::warn!(
                    pool_key = entry_key,
                    idle_secs = idle.as_secs(),
                    "downstream MCP idle session expired; closing transport"
                );
            }
            !closed && (e.in_flight.load(Ordering::Relaxed) > 0 || idle < IDLE_TTL)
        });
        if let Some(entry) = map.get_mut(&k) {
            entry.last_used = now;
            entry.in_flight.fetch_add(1, Ordering::Relaxed);
            return Ok(AcquiredSession {
                service: entry.service.clone(),
                _lease: InFlightGuard {
                    key: k,
                    in_flight: entry.in_flight.clone(),
                },
            });
        }
    }

    // Serialize the cold open per key: concurrent first calls share one child
    // instead of each spawning their own.
    let gate = connect_gate(k);
    let _connect_permit = gate.lock().await;
    {
        let mut map = lock();
        if let Some(entry) = map.get_mut(&k) {
            if !entry.service.is_closed() && !entry.service.is_transport_closed() {
                // The gate winner opened the session while we waited.
                entry.last_used = Instant::now();
                entry.in_flight.fetch_add(1, Ordering::Relaxed);
                return Ok(AcquiredSession {
                    service: entry.service.clone(),
                    _lease: InFlightGuard {
                        key: k,
                        in_flight: entry.in_flight.clone(),
                    },
                });
            }
            tracing::warn!(
                pool_key = k,
                "downstream MCP transport closed while waiting to connect; reopening"
            );
            map.remove(&k);
        }
    }

    // Open outside the lock: a slow connect must not block other servers, and we
    // never hold a std Mutex across an await.
    let service = Arc::new(client::open(transport, timeout).await?);
    let mut map = lock();
    // Unreachable for same-key racers while the gate is held; kept as a
    // belt-and-braces guard for any future path that skips the gate.
    if map.contains_key(&k) {
        tracing::warn!(
            pool_key = k,
            "concurrent downstream MCP connection replaced an existing pooled session"
        );
    }
    let in_flight = Arc::new(AtomicU32::new(1));
    map.insert(
        k,
        Entry {
            service: service.clone(),
            last_used: Instant::now(),
            in_flight: in_flight.clone(),
        },
    );
    Ok(AcquiredSession {
        service,
        _lease: InFlightGuard { key: k, in_flight },
    })
}

pub struct AcquiredSession {
    pub service: Arc<ClientService>,
    _lease: InFlightGuard,
}

/// Releases an acquired session when its request future is cancelled or exits
/// early. Without this guard, cancellation can leave `in_flight` nonzero forever
/// and prevent the idle sweep from reclaiming the session.
pub struct InFlightGuard {
    key: u64,
    in_flight: Arc<AtomicU32>,
}

fn owns_current_session(current: &Arc<AtomicU32>, lease_session: &Arc<AtomicU32>) -> bool {
    Arc::ptr_eq(current, lease_session)
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        let mut map = lock();
        if let Some(entry) = map.get_mut(&self.key)
            && owns_current_session(&entry.in_flight, &self.in_flight)
        {
            entry.last_used = Instant::now();
        }
    }
}

/// Drop the pooled session for `key` (e.g. after a transport-level failure), so
/// the next [`acquire`] reopens a fresh one.
pub fn evict(key: u64) {
    if lock().remove(&key).is_some() {
        tracing::warn!(
            pool_key = key,
            "downstream MCP session evicted after failed request"
        );
    }
}

/// Drop every pooled session (closing all children). Called when the gateway
/// wiring changes (install/remove/revoke → [`super::catalog::invalidate`]).
pub fn clear() {
    let mut map = lock();
    if !map.is_empty() {
        tracing::warn!(
            sessions = map.len(),
            "downstream MCP pool cleared by configuration/catalog invalidation"
        );
    }
    map.clear();
}

/// Number of live pooled sessions (test/diagnostic helper).
#[must_use]
pub fn len() -> usize {
    lock().len()
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<u64, Entry>> {
    // A poisoned lock only means a previous holder panicked mid-map-op; the map
    // is still structurally valid, so recover rather than propagate the panic.
    pool()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn stdio(cmd: &str) -> ResolvedTransport {
        ResolvedTransport::Stdio {
            command: cmd.into(),
            args: vec![],
            env: BTreeMap::new(),
            binary_sha256: String::new(),
        }
    }

    #[test]
    fn key_is_stable_and_wiring_sensitive() {
        assert_eq!(key(&stdio("a")), key(&stdio("a")), "same wiring → same key");
        assert_ne!(
            key(&stdio("a")),
            key(&stdio("b")),
            "different command → different key"
        );
    }

    #[test]
    fn http_key_identity_uses_secret_fingerprint_not_raw_value() {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".into(), "Bearer raw-token-one".into());
        let mut secrets = BTreeMap::new();
        secrets.insert("Authorization".into(), "fp-one".into());
        let first = ResolvedTransport::Http {
            url: "https://gitlab.example/mcp".into(),
            headers: headers.clone(),
            secret_fingerprints: secrets.clone(),
            oauth: false,
        };
        assert!(!pool_identity(&first).contains("raw-token-one"));
        headers.insert("Authorization".into(), "Bearer raw-token-two".into());
        let same_fingerprint = ResolvedTransport::Http {
            url: "https://gitlab.example/mcp".into(),
            headers,
            secret_fingerprints: secrets.clone(),
            oauth: false,
        };
        assert_eq!(key(&first), key(&same_fingerprint));
        secrets.insert("Authorization".into(), "fp-two".into());
        let rotated = ResolvedTransport::Http {
            url: "https://gitlab.example/mcp".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer raw-token-two".into())]),
            secret_fingerprints: secrets,
            oauth: false,
        };
        assert_ne!(key(&first), key(&rotated));
    }

    #[test]
    fn http_key_treats_secret_header_names_case_insensitively() {
        let upper = ResolvedTransport::Http {
            url: "https://example.com/mcp".into(),
            headers: BTreeMap::from([("Authorization".into(), "private-token".into())]),
            secret_fingerprints: BTreeMap::from([("Authorization".into(), "fingerprint".into())]),
            oauth: false,
        };
        let lower_fingerprint = ResolvedTransport::Http {
            url: "https://example.com/mcp".into(),
            headers: BTreeMap::from([("Authorization".into(), "private-token".into())]),
            secret_fingerprints: BTreeMap::from([("authorization".into(), "fingerprint".into())]),
            oauth: false,
        };

        assert!(!pool_identity(&lower_fingerprint).contains("private-token"));
        assert_eq!(key(&upper), key(&lower_fingerprint));
    }

    #[test]
    fn key_changes_when_secret_format_changes() {
        use super::super::config::{GatewayServer, SecretMementoRef, TransportKind};
        use super::super::memento::SecretMementoStore;

        let store = SecretMementoStore::global();
        store.put("mcp/test/pool-format", "token");
        let mut server = GatewayServer {
            name: "remote".into(),
            transport: TransportKind::Http,
            url: "https://example.com/mcp".into(),
            secret_headers: BTreeMap::from([(
                "Authorization".into(),
                SecretMementoRef {
                    id: "mcp/test/pool-format".into(),
                    format: String::new(),
                },
            )]),
            ..Default::default()
        };

        let raw = server.resolve().expect("resolve raw secret");
        server
            .secret_headers
            .get_mut("Authorization")
            .expect("secret header")
            .format = "Bearer {secret}".into();
        let bearer = server.resolve().expect("resolve formatted secret");
        store.remove("mcp/test/pool-format");

        assert_ne!(key(&raw), key(&bearer));
    }

    #[test]
    fn evict_and_clear_are_safe_when_empty() {
        clear();
        evict(key(&stdio("never-pooled")));
        clear();
        assert_eq!(len(), 0);
    }

    #[test]
    fn dropping_cancelled_lease_releases_its_session_count_only() {
        let old_session_count = Arc::new(AtomicU32::new(1));
        let replacement_count = Arc::new(AtomicU32::new(1));
        let lease = InFlightGuard {
            key: u64::MAX,
            in_flight: old_session_count.clone(),
        };

        // Dropping the request future drops its lease. If the pool has since
        // replaced this key, the old lease still only owns the old counter.
        drop(lease);

        assert_eq!(old_session_count.load(Ordering::Relaxed), 0);
        assert_eq!(replacement_count.load(Ordering::Relaxed), 1);
        assert!(!owns_current_session(
            &replacement_count,
            &old_session_count
        ));
    }
}
