//! Production [`ChainQuery`] implementation over Sui JSON-RPC (`suix_getOwnedObjects`,
//! `suix_queryEvents`, `sui_getTransactionBlock`). The RPC round-trips are not unit-tested
//! (network-dependent) — covered by the localnet integration loop; the pure match/parse
//! helpers are unit-tested.

use crate::http_client::HttpClient;
use crate::verify::ChainQuery;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A cached result of an ownership query.
struct CacheEntry {
    /// Whether the address owned the NFT at query time.
    owns: bool,
    /// Monotonic expiry; stale entries are ignored and refreshed on next access.
    expiry: Instant,
}

/// Production [`ChainQuery`] implementation backed by Sui JSON-RPC.
pub struct SuiRpc {
    /// Shared HTTP client for all RPC calls.
    client: HttpClient,
    /// Sui JSON-RPC endpoint URL.
    rpc_url: String,
    /// Ownership-cache TTL. 0 = disabled (every gated check is live on-chain).
    cache_ttl: Duration,
    /// Optional ownership cache, keyed by `address|nft_type|gate_id`.
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl SuiRpc {
    /// Create a new `SuiRpc` client. Set `cache_ttl_ms` to `0` to disable the ownership cache
    /// (the default — every gated check hits the RPC live).
    pub fn new(client: HttpClient, rpc_url: String, cache_ttl_ms: u64) -> Self {
        Self {
            client,
            rpc_url,
            cache_ttl: Duration::from_millis(cache_ttl_ms),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Send a JSON-RPC request to the configured endpoint and return the `result` field.
    ///
    /// # Errors
    ///
    /// Returns an error on HTTP failure, JSON parse failure, or if the RPC response contains an
    /// `error` field.
    async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = self.client.post_json(&self.rpc_url, &body).await?;
        if let Some(err) = resp.get("error") {
            anyhow::bail!("rpc error from {method}: {err}");
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Derive the package id from a `<pkg>::module::Type` string.
    fn package_of(nft_type: &str) -> Option<&str> {
        nft_type.split("::").next()
    }

    /// Build a stable cache key from the three ownership-query parameters.
    fn cache_key(address: &str, nft_type: &str, gate_id: Option<&str>) -> String {
        format!("{address}|{nft_type}|{}", gate_id.unwrap_or("-"))
    }

    /// The uncached, live ownership query.
    async fn owns_nft_live(
        &self,
        address: &str,
        nft_type: &str,
        gate_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        let params = json!([
            address,
            { "filter": { "StructType": nft_type }, "options": { "showContent": true } },
            null,
            50
        ]);
        let result = self.call("suix_getOwnedObjects", params).await?;
        let empty = vec![];
        let data = result
            .get("data")
            .and_then(|d| d.as_array())
            .unwrap_or(&empty);
        if gate_id.is_none() {
            return Ok(!data.is_empty());
        }
        let want = gate_id.unwrap();
        Ok(data.iter().any(|entry| {
            entry
                .pointer("/data/content/fields/data/fields/gate_id")
                .and_then(|v| v.as_str())
                == Some(want)
        }))
    }

    /// Directly verify the `consume` transaction named by `digest`: it must have executed and
    /// emitted an `AccessConsumedEvent` matching `nonce` (and `gate_id`), sent by `address`.
    /// Defence-in-depth atop the sender+nonce event query.
    async fn consume_tx_matches(
        &self,
        digest: &str,
        nonce: &str,
        address: &str,
        gate_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        let params = json!([digest, { "showEvents": true, "showEffects": true }]);
        let result = self.call("sui_getTransactionBlock", params).await?;
        // Must have succeeded.
        let status_ok = result
            .pointer("/effects/status/status")
            .and_then(|v| v.as_str())
            .map(|s| s == "success")
            .unwrap_or(false);
        if !status_ok {
            return Ok(false);
        }
        let empty = vec![];
        let events = result
            .get("events")
            .and_then(|e| e.as_array())
            .unwrap_or(&empty);
        Ok(events
            .iter()
            .any(|ev| is_consumed_event(ev) && event_matches(ev, nonce, address, gate_id)))
    }
}

/// True if the event's type is an `access_gate::AccessConsumedEvent`.
fn is_consumed_event(ev: &Value) -> bool {
    ev.get("type")
        .and_then(|v| v.as_str())
        .map(|t| t.ends_with("::access_gate::AccessConsumedEvent"))
        .unwrap_or(false)
}

/// UTF-8 bytes of `nonce`, as the on-chain `AccessConsumedEvent.nonce` (a `vector<u8>`) is
/// rendered by RPC — usually an array of byte numbers, sometimes base64.
fn nonce_matches(event_nonce: &Value, nonce: &str) -> bool {
    let want: Vec<u64> = nonce.as_bytes().iter().map(|b| *b as u64).collect();
    match event_nonce {
        Value::Array(arr) => {
            arr.len() == want.len()
                && arr
                    .iter()
                    .zip(want.iter())
                    .all(|(a, w)| a.as_u64() == Some(*w))
        }
        Value::String(s) => {
            use base64::prelude::*;
            BASE64_STANDARD
                .decode(s)
                .map(|d| d == nonce.as_bytes())
                .unwrap_or(false)
        }
        _ => false,
    }
}

/// Does a single event match this challenge: sender == address, nonce matches, and (if given)
/// gate_id matches. (When the query is already sender-filtered the sender check is redundant
/// but kept as defence in depth.)
fn event_matches(ev: &Value, nonce: &str, address: &str, gate_id: Option<&str>) -> bool {
    let sender_ok = ev.get("sender").and_then(|v| v.as_str()) == Some(address);
    let nonce_ok = ev
        .pointer("/parsedJson/nonce")
        .map(|v| nonce_matches(v, nonce))
        .unwrap_or(false);
    let gate_ok = match gate_id {
        Some(g) => ev.pointer("/parsedJson/gate_id").and_then(|v| v.as_str()) == Some(g),
        None => true,
    };
    sender_ok && nonce_ok && gate_ok
}

// `ChainQuery` implementation for `SuiRpc` — production on-chain lookups via Sui JSON-RPC.
impl ChainQuery for SuiRpc {
    async fn owns_nft(
        &self,
        address: &str,
        nft_type: &str,
        gate_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        // Cache is OFF by default (cache_ttl == 0) so a gated action is confirmed live. When
        // the operator opts into a small TTL, collapse duplicate lookups within that window.
        if !self.cache_ttl.is_zero() {
            let key = Self::cache_key(address, nft_type, gate_id);
            if let Some(e) = self.cache.lock().unwrap().get(&key) {
                if e.expiry > Instant::now() {
                    return Ok(e.owns);
                }
            }
            let owns = self.owns_nft_live(address, nft_type, gate_id).await?;
            self.cache.lock().unwrap().insert(
                key,
                CacheEntry {
                    owns,
                    expiry: Instant::now() + self.cache_ttl,
                },
            );
            return Ok(owns);
        }
        self.owns_nft_live(address, nft_type, gate_id).await
    }

    async fn consume_event_matches(
        &self,
        nonce: &str,
        address: &str,
        nft_type: &str,
        gate_id: Option<&str>,
        consume_digest: Option<&str>,
    ) -> anyhow::Result<bool> {
        let pkg = Self::package_of(nft_type)
            .ok_or_else(|| anyhow::anyhow!("cannot derive package from nft_type"))?;
        let event_type = format!("{pkg}::access_gate::AccessConsumedEvent");
        // F5: filter by BOTH the event type AND the emitting Sender, so the scan is O(this
        // user's consume events) rather than a flat recent-N over all users.
        let params = json!([
            { "All": [ { "MoveEventType": event_type }, { "Sender": address } ] },
            null,
            50,
            true // descending: most recent first
        ]);
        let result = self.call("suix_queryEvents", params).await?;
        let empty = vec![];
        let data = result
            .get("data")
            .and_then(|d| d.as_array())
            .unwrap_or(&empty);
        let primary = data
            .iter()
            .any(|ev| event_matches(ev, nonce, address, gate_id));
        if !primary {
            return Ok(false);
        }
        // F4: if a consume tx digest was supplied, verify that exact transaction too.
        if let Some(digest) = consume_digest {
            return self
                .consume_tx_matches(digest, nonce, address, gate_id)
                .await;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nonce_matches_byte_array() {
        assert!(nonce_matches(&json!([110, 111, 110, 99, 101]), "nonce"));
        assert!(!nonce_matches(&json!([1, 2, 3]), "nonce"));
    }

    #[test]
    fn nonce_matches_base64() {
        use base64::prelude::*;
        let b64 = BASE64_STANDARD.encode("nonce");
        assert!(nonce_matches(&json!(b64), "nonce"));
    }

    #[test]
    fn package_of_extracts_prefix() {
        assert_eq!(
            SuiRpc::package_of("0xabc::access_gate::AccessNFT"),
            Some("0xabc")
        );
    }

    #[test]
    fn event_matches_checks_sender_nonce_gate() {
        let ev = json!({
            "type": "0xabc::access_gate::AccessConsumedEvent",
            "sender": "0xowner",
            "parsedJson": { "nonce": [110, 111, 110, 99, 101], "gate_id": "0xgate" }
        });
        assert!(is_consumed_event(&ev));
        assert!(event_matches(&ev, "nonce", "0xowner", Some("0xgate")));
        assert!(!event_matches(&ev, "nonce", "0xattacker", Some("0xgate"))); // wrong sender
        assert!(!event_matches(&ev, "other", "0xowner", Some("0xgate"))); // wrong nonce
        assert!(!event_matches(&ev, "nonce", "0xowner", Some("0xwrong"))); // wrong gate
        assert!(event_matches(&ev, "nonce", "0xowner", None)); // gate not constrained
    }

    #[test]
    fn cache_key_is_stable() {
        assert_eq!(SuiRpc::cache_key("0xa", "0xt", Some("0xg")), "0xa|0xt|0xg");
        assert_eq!(SuiRpc::cache_key("0xa", "0xt", None), "0xa|0xt|-");
    }
}
