//! Challenge / nonce store. Issues random, time-bound nonces and lets a valid nonce be
//! consumed exactly once (replay protection).
//!
//! Two backends behind one async [`NonceStore`] enum:
//! - [`InMemoryNonceStore`] — process-local `Mutex<HashMap>`, hardened with a **hard entry
//!   cap** (evict-oldest) and a **background prune** so growth is bounded independent of issue
//!   cadence (gateway audit F3). Correct only at `replicas: 1` (F2).
//! - [`RedisNonceStore`] — a shared TTL store (Redis protocol; works with **Dragonfly**) so
//!   nonce consumption is fleet-wide and replay protection survives horizontal scale-out
//!   (gateway audit F2). Single-use is atomic via `GETDEL`; expiry is the key TTL.
//!
//! Both fail **closed**: on any backend error, `take_if_valid` returns `false`.

use rand_core::{OsRng, RngCore};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Current Unix time in milliseconds. Used to compute nonce expiry timestamps.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Generate a cryptographically random 24-byte hex nonce (48 hex chars). Matches the Workers
/// gateway's `randomHex24()` entropy.
fn random_nonce() -> String {
    let mut buf = [0u8; 24];
    OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

// ── In-memory backend ────────────────────────────────────────────────────────────

/// A single tracked nonce in the in-memory store.
struct Entry {
    /// When this nonce expires (monotonic).
    expiry: Instant,
    /// True once the nonce has been consumed (single-use enforcement).
    used: bool,
}

/// Process-local nonce store backed by a `Mutex<HashMap>`.
pub struct InMemoryNonceStore {
    /// Lifetime of each issued nonce.
    ttl: Duration,
    /// Hard cap on live entries; evicts the soonest-to-expire entry when full (audit F3).
    max_entries: usize,
    /// The live nonce map, protected by a mutex for multi-threaded access.
    inner: Mutex<HashMap<String, Entry>>,
}

impl InMemoryNonceStore {
    /// Create a new store with the given TTL and entry cap.
    pub fn new(ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_secs),
            max_entries: max_entries.max(1),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Insert `nonce` and return `(nonce, expires_at_unix_ms)`. Evicts expired entries and, if
    /// still full, the soonest-to-expire live entry before inserting.
    fn insert(&self, nonce: String) -> (String, u64) {
        let now = Instant::now();
        let expiry = now + self.ttl;
        let mut m = self.inner.lock().unwrap();
        m.retain(|_, e| e.expiry > now);
        // Hard cap: if still full after pruning expired, evict the soonest-to-expire entry so
        // memory is bounded regardless of issue rate (F3).
        if m.len() >= self.max_entries {
            if let Some(oldest) = m
                .iter()
                .min_by_key(|(_, e)| e.expiry)
                .map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            }
        }
        m.insert(
            nonce.clone(),
            Entry {
                expiry,
                used: false,
            },
        );
        (nonce, now_ms() + self.ttl.as_millis() as u64)
    }

    /// Mark `nonce` as used. Returns `true` iff it was present, unexpired, and not yet used.
    fn take_if_valid(&self, nonce: &str) -> bool {
        let mut m = self.inner.lock().unwrap();
        match m.get_mut(nonce) {
            Some(e) if !e.used && e.expiry > Instant::now() => {
                e.used = true;
                true
            }
            _ => false,
        }
    }

    /// Drop expired entries. Called periodically by the background prune task.
    pub fn prune(&self) {
        let now = Instant::now();
        self.inner.lock().unwrap().retain(|_, e| e.expiry > now);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

// ── Redis / Dragonfly backend ───────────────────────────────────────────────────

/// Shared nonce store backed by a Redis-protocol server (Redis or Dragonfly).
pub struct RedisNonceStore {
    /// Multiplexed async connection manager; cloned cheaply per command.
    conn: redis::aio::ConnectionManager,
    /// Nonce TTL in milliseconds (used for `SET … PX`).
    ttl_ms: u64,
    /// Key prefix applied to every stored nonce (`nftgate:nonce:`).
    prefix: String,
}

impl RedisNonceStore {
    /// Connect to a Redis-protocol server (Redis or Dragonfly). Startup-time async.
    pub async fn connect(url: &str, ttl_secs: u64) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        let conn = client.get_connection_manager().await?;
        Ok(Self {
            conn,
            ttl_ms: ttl_secs.saturating_mul(1000),
            prefix: "nftgate:nonce:".to_string(),
        })
    }

    /// Build the full Redis key for `nonce`.
    fn key(&self, nonce: &str) -> String {
        format!("{}{}", self.prefix, nonce)
    }

    /// Store `nonce` with a key TTL of `ttl_ms`. Best-effort: logs a warning on failure but
    /// does not abort — `take_if_valid` will then fail closed on the missing key.
    async fn insert(&self, nonce: String) -> (String, u64) {
        let mut c = self.conn.clone();
        // SET key 1 PX <ttl> — random nonces don't collide, so NX is unnecessary.
        let res: redis::RedisResult<()> = redis::cmd("SET")
            .arg(self.key(&nonce))
            .arg(1)
            .arg("PX")
            .arg(self.ttl_ms)
            .query_async(&mut c)
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "redis nonce SET failed (issue best-effort; take will fail closed)");
        }
        (nonce, now_ms() + self.ttl_ms)
    }

    /// Atomically consume `nonce` via `GETDEL` (Redis ≥6.2 / Dragonfly). Returns `false` on a
    /// missing/expired key or a Redis error (fails closed).
    async fn take_if_valid(&self, nonce: &str) -> bool {
        let mut c = self.conn.clone();
        // GETDEL is atomic: returns the value and deletes the key in one step, so a nonce is
        // consumed exactly once fleet-wide; a missing/expired key returns nil.
        let res: redis::RedisResult<Option<String>> = redis::cmd("GETDEL")
            .arg(self.key(nonce))
            .query_async(&mut c)
            .await;
        match res {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "redis nonce GETDEL failed; failing closed");
                false
            }
        }
    }
}

// ── Unified async enum ───────────────────────────────────────────────────────────

/// Unified async nonce store — either the process-local [`InMemoryNonceStore`] or the
/// shared [`RedisNonceStore`]. Selected at startup by the presence of `REDIS_URL`.
pub enum NonceStore {
    /// Process-local store (correct only at `replicas: 1`).
    InMemory(InMemoryNonceStore),
    /// Shared Redis/Dragonfly store (fleet-wide replay protection).
    Redis(RedisNonceStore),
}

impl NonceStore {
    /// Create an in-memory store with the given TTL and hard entry cap.
    pub fn in_memory(ttl_secs: u64, max_entries: usize) -> Self {
        NonceStore::InMemory(InMemoryNonceStore::new(ttl_secs, max_entries))
    }

    /// Connect to a Redis-protocol nonce store. Async startup — returns an error if the
    /// connection cannot be established.
    pub async fn redis(url: &str, ttl_secs: u64) -> anyhow::Result<Self> {
        Ok(NonceStore::Redis(
            RedisNonceStore::connect(url, ttl_secs).await?,
        ))
    }

    /// Issue a fresh random nonce. Returns `(nonce, expires_at_unix_ms)`.
    pub async fn issue(&self) -> (String, u64) {
        let nonce = random_nonce();
        match self {
            NonceStore::InMemory(s) => s.insert(nonce),
            NonceStore::Redis(s) => s.insert(nonce).await,
        }
    }

    /// Consume a nonce exactly once; true iff it was valid, unexpired, and unused.
    pub async fn take_if_valid(&self, nonce: &str) -> bool {
        match self {
            NonceStore::InMemory(s) => s.take_if_valid(nonce),
            NonceStore::Redis(s) => s.take_if_valid(nonce).await,
        }
    }

    /// Prune expired entries (in-memory only; Redis expires via TTL). No-op for Redis.
    pub fn prune(&self) {
        if let NonceStore::InMemory(s) = self {
            s.prune();
        }
    }

    /// Insert a caller-chosen nonce (test helper). Not available in production builds.
    #[cfg(test)]
    pub async fn issue_specific(&self, nonce: &str) -> (String, u64) {
        match self {
            NonceStore::InMemory(s) => s.insert(nonce.to_string()),
            NonceStore::Redis(s) => s.insert(nonce.to_string()).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nonce_valid_once_then_used() {
        let store = NonceStore::in_memory(300, 10_000);
        let (nonce, _) = store.issue().await;
        assert!(store.take_if_valid(&nonce).await);
        assert!(!store.take_if_valid(&nonce).await); // already used
    }

    #[tokio::test]
    async fn unknown_nonce_rejected() {
        let store = NonceStore::in_memory(300, 10_000);
        assert!(!store.take_if_valid("never-issued").await);
    }

    #[tokio::test]
    async fn expired_nonce_rejected() {
        let store = NonceStore::in_memory(0, 10_000); // immediate expiry
        let (nonce, _) = store.issue().await;
        assert!(!store.take_if_valid(&nonce).await);
    }

    #[tokio::test]
    async fn hard_cap_bounds_memory() {
        let store = NonceStore::in_memory(300, 4); // cap 4
        for _ in 0..20 {
            let _ = store.issue().await;
        }
        if let NonceStore::InMemory(s) = &store {
            assert!(s.len() <= 4, "entry count must stay within the hard cap");
        }
    }

    #[tokio::test]
    async fn prune_drops_expired() {
        let store = NonceStore::in_memory(0, 10_000);
        let _ = store.issue().await;
        store.prune();
        if let NonceStore::InMemory(s) = &store {
            assert_eq!(s.len(), 0);
        }
    }
}
