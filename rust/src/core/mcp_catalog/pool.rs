//! Persistent downstream MCP session pool (#1078).
//!
//! Without pooling, every `ctx_tools` list/call reopened a connection — spawning
//! a fresh child process and re-running the MCP `initialize` handshake — so each
//! tool call paid the full spawn+handshake latency. This pool keeps one live
//! [`ClientService`] per distinct wiring and reuses it across calls:
//!
//! - **keyed** by the resolved transport (same command/args/env/caps/url → same
//!   session), so two different servers never share a child,
//! - **idle-evicted**: a session unused for `IDLE_TTL` is dropped on the next
//!   access (closing the child's stdin → the server exits), swept opportunistically
//!   on every [`acquire`],
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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::client::{self, ClientService};
use super::config::ResolvedTransport;

/// Drop a pooled session after this long without use, so an idle child process
/// does not linger. The next call for that wiring transparently reopens.
const IDLE_TTL: Duration = Duration::from_mins(5);

struct Entry {
    service: Arc<ClientService>,
    last_used: Instant,
}

fn pool() -> &'static Mutex<HashMap<u64, Entry>> {
    static POOL: OnceLock<Mutex<HashMap<u64, Entry>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
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
) -> Result<Arc<ClientService>, String> {
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
            let closed = e.service.is_closed();
            if closed {
                tracing::warn!(
                    pool_key = entry_key,
                    "downstream MCP transport closed; evicting session"
                );
            } else if idle >= IDLE_TTL {
                tracing::warn!(
                    pool_key = entry_key,
                    idle_secs = idle.as_secs(),
                    "downstream MCP idle session expired; closing transport"
                );
            }
            !closed && idle < IDLE_TTL
        });
        if let Some(entry) = map.get_mut(&k) {
            entry.last_used = now;
            return Ok(entry.service.clone());
        }
    }

    // Open outside the lock: a slow connect must not block other servers, and we
    // never hold a std Mutex across an await.
    let service = Arc::new(client::open(transport, timeout).await?);
    let mut map = lock();
    // A racing first-call may have inserted already; last writer wins and the
    // loser's child is closed when its Arc drops.
    if map.contains_key(&k) {
        tracing::warn!(
            pool_key = k,
            "concurrent downstream MCP connection replaced an existing pooled session"
        );
    }
    map.insert(
        k,
        Entry {
            service: service.clone(),
            last_used: Instant::now(),
        },
    );
    Ok(service)
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
}
