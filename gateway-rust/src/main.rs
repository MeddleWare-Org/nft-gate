//! Generic NFT-gated reverse proxy binary.
//!
//! # Startup sequence
//!
//! 1. Load [`config::GatewayConfig`] from environment variables (see `config.rs`). Abort on
//!    missing required vars.
//! 2. Build an [`http_client::HttpClient`] for upstream and Sui RPC HTTP.
//! 3. Construct a [`sui_rpc::SuiRpc`] from the shared client.
//! 4. Open either an in-memory or Redis [`challenge::NonceStore`] based on `REDIS_URL`.
//! 5. Create the per-address [`ratelimit::RateLimiter`].
//! 6. Spawn a background task that prunes the in-memory nonce store every
//!    `NONCE_PRUNE_INTERVAL_SECS` (no-op for the Redis backend).
//! 7. Bind an axum TCP listener and serve with graceful shutdown on SIGINT.
//!
//! # AppState
//!
//! [`AppState`] is the single shared object threaded through every request via
//! `axum::extract::State`. It holds:
//!
//! - `cfg` — the loaded gateway configuration.
//! - `store` — the nonce store (in-memory or Redis).
//! - `limiter` — the per-address, per-minute rate limiter.
//! - `chain` — the Sui RPC client for ownership / event queries.
//! - `http` — the shared [`http_client::HttpClient`] used both by the proxy and the RPC client.

mod challenge;
mod config;
mod http_client;
mod proof;
mod proxy;
mod ratelimit;
mod sui_rpc;
mod verify;

use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde_json::json;
use std::sync::Arc;

use challenge::NonceStore;
use config::GatewayConfig;
use http_client::HttpClient;
use ratelimit::RateLimiter;
use sui_rpc::SuiRpc;
use verify::verify_access_request;

/// Shared state threaded through every axum request handler.
pub struct AppState {
    /// Loaded gateway configuration (env vars).
    cfg: GatewayConfig,
    /// Single-use nonce store (in-memory or Redis).
    store: NonceStore,
    /// Per-address, fixed-window rate limiter.
    limiter: RateLimiter,
    /// Sui JSON-RPC client for ownership and consume-event queries.
    chain: SuiRpc,
    /// Shared HTTP client used by both the reverse proxy and the Sui RPC calls.
    http: HttpClient,
}

/// Build a JSON `{"error": reason}` response with the given status code.
fn deny(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({ "error": reason }))).into_response()
}

/// Extract the base64 access-proof token from the request. Prefers `Authorization: Bearer
/// <token>`; falls back to the `X-Access-Proof` header.
fn extract_proof_token(req: &Request) -> Option<String> {
    // Prefer `Authorization: Bearer <token>` (the conventional client channel); fall back to
    // an explicit `X-Access-Proof` header.
    if let Some(v) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(s) = v.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                return Some(rest.trim().to_string());
            }
        }
    }
    req.headers()
        .get("x-access-proof")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

/// Main request dispatcher. Handles `/healthz`, `/v1/challenge`, configured public paths, and
/// gated paths (signature + ownership verification before forwarding to the upstream).
async fn handle(State(app): State<Arc<AppState>>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if path == "/healthz" {
        return (StatusCode::OK, "ok").into_response();
    }

    if method == Method::GET && path == "/v1/challenge" {
        let (nonce, expires_at) = app.store.issue().await;
        return Json(json!({ "nonce": nonce, "expiresAt": expires_at })).into_response();
    }

    // Public passthrough (e.g. /v1/tip-config): forward without auth.
    if app.cfg.is_public_path(&path) {
        return proxy::forward(&app, req).await;
    }

    let token = match extract_proof_token(&req) {
        Some(t) => t,
        None => return deny(StatusCode::UNAUTHORIZED, "missing access proof"),
    };

    match verify_access_request(&app.cfg, &app.store, &token, &app.chain).await {
        Ok(address) => {
            if !app.limiter.check(&address) {
                return deny(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded");
            }
            proxy::forward(&app, req).await
        }
        Err(d) => {
            let status = match d {
                verify::Denied::ChainError => StatusCode::BAD_GATEWAY,
                _ => StatusCode::FORBIDDEN,
            };
            deny(status, d.reason())
        }
    }
}

/// Construct the axum [`Router`] with the shared [`AppState`]. All routes are handled by
/// the single [`handle`] fallback.
fn build_router(state: Arc<AppState>) -> Router {
    Router::new().fallback(any(handle)).with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = GatewayConfig::from_env()?;
    tracing::info!(?cfg.bind_addr, upstream = %cfg.upstream_url, nft_type = %cfg.nft_type, single_use = cfg.single_use, "starting nft-gate-gateway");

    let http = HttpClient::new()?;
    let chain = SuiRpc::new(
        http.clone(),
        cfg.sui_rpc_url.clone(),
        cfg.ownership_cache_ttl_ms,
    );
    let store = match &cfg.redis_url {
        Some(url) => {
            tracing::info!("using Redis/Dragonfly nonce store — fleet-wide replay protection");
            NonceStore::redis(url, cfg.challenge_ttl_secs).await?
        }
        None => {
            tracing::info!(
                "using in-memory nonce store (correct at replicas:1; set REDIS_URL to scale out)"
            );
            NonceStore::in_memory(cfg.challenge_ttl_secs, cfg.nonce_max_entries)
        }
    };
    let limiter = RateLimiter::new(cfg.rate_limit_per_min);
    let bind_addr = cfg.bind_addr;
    let prune_interval = cfg.nonce_prune_interval_secs.max(1);

    let state = Arc::new(AppState {
        cfg,
        store,
        limiter,
        chain,
        http,
    });

    // Background prune keeps the in-memory nonce map bounded independent of issue cadence
    // (no-op for the Redis backend, which expires via key TTL).
    {
        let prune_state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(prune_interval));
            loop {
                ticker.tick().await;
                prune_state.store.prune();
            }
        });
    }

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolves when SIGINT (Ctrl-C) is received, triggering axum's graceful shutdown.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_path_matching() {
        let mut cfg = test_cfg();
        cfg.public_paths = vec!["/v1/tip-config".into()];
        assert!(cfg.is_public_path("/v1/tip-config"));
        assert!(!cfg.is_public_path("/v1/blob-upload"));
    }

    fn test_cfg() -> GatewayConfig {
        GatewayConfig {
            bind_addr: "0.0.0.0:8080".parse().unwrap(),
            upstream_url: "http://u".into(),
            sui_rpc_url: "http://r".into(),
            nft_type: "0x1::access_gate::AccessNFT".into(),
            gate_id: None,
            challenge_ttl_secs: 300,
            single_use: false,
            public_paths: vec![],
            rate_limit_per_min: 30,
            max_body_bytes: 262144,
            redis_url: None,
            nonce_max_entries: 10_000,
            nonce_prune_interval_secs: 60,
            ownership_cache_ttl_ms: 0,
        }
    }
}
