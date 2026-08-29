# CLAUDE.md — gateway-rust

## What this crate is

The Rust/Axum implementation of the nft-gate gateway. Wire-identical to `gateway-workers/`
— same routes, status codes, proof format, Sui RPC calls, and env-var config. Deployed as
a Docker container or a Cargo binary; state backed by an in-memory store (single replica) or
Redis/Dragonfly (horizontal scale-out).

## Axum architecture

`AppState` is the single shared object (`Arc<AppState>`) threaded through every request handler:
- `cfg: GatewayConfig` — loaded from env vars at startup; immutable for the process lifetime.
- `store: NonceStore` — in-memory or Redis, depending on `REDIS_URL`.
- `limiter: RateLimiter` — `Mutex<HashMap>`, per-address fixed 60s window.
- `chain: SuiRpc` — Sui JSON-RPC client; optional in-memory ownership cache.
- `http: HttpClient` — shared `hyper` client used by both the proxy and the RPC client.

All routes are dispatched through a single `fallback` handler (`handle`). The router
does not enumerate paths — the handler checks them in order: `/healthz` → `/v1/challenge`
→ public paths → gated paths.

## Nonce store options

**In-memory** (default; `REDIS_URL` unset)
- `Mutex<HashMap<String, Entry>>` — correct at `replicas: 1`.
- Hard entry cap (`NONCE_MAX_ENTRIES`, default 1,000,000): when full, evict the soonest-to-expire
  entry before inserting (audit F3 — bounds memory independent of issue rate).
- Background prune task (`NONCE_PRUNE_INTERVAL_SECS`, default 60s) drops expired entries.
- **Not safe at replicas > 1** — different replicas have independent maps; a nonce issued by
  replica A can be replayed on replica B. Set `REDIS_URL` for scale-out.

**Redis / Dragonfly** (`REDIS_URL` set)
- `GETDEL` is atomic: a nonce is consumed exactly once fleet-wide (no replay across replicas).
- Expiry is the Redis key TTL; no background prune needed.
- Requires Redis ≥ 6.2 or Dragonfly for `GETDEL`. Older Redis needs `GET` + `DEL` (non-atomic —
  upgrade or use Dragonfly).
- `RedisNonceStore::insert` fails best-effort (logs warning; `take_if_valid` then fails closed
  on the missing key).

## HTTP client

Uses `hyper 1.x + hyper-rustls` directly (not reqwest) to avoid the `url → idna → icu_normalizer`
compile-time dep chain (~10 min on GHA). All URLs are operator-configured ASCII domains —
`hyper::Uri` parses them without IDNA processing.

## Deployment targets

- **Docker:** Dockerfile uses a multi-stage build (builder → distroless). Published to GHCR
  via the publish workflow.
- **Kubernetes:** See `infrastructure/k8s/` in the vault monorepo for k8s manifests.
- **Bare metal / VM:** `cargo install nft-gate-gateway` or `cargo build --release`.

## Ownership cache

`OWNERSHIP_CACHE_TTL_MS` defaults to `0` (disabled — every gated check is live on-chain).
A small positive value collapses duplicate RPC lookups under load at the cost of a brief
staleness window on NFT transfer/burn. Never applied to single-use consume-event checks (always
live). Use with care on mainnet — a stolen/transferred NFT can still gate during the TTL window.

## Invariants

- `deny` always returns JSON `{"error": reason}`. Do not return plain-text error bodies.
- `extract_proof_token` prefers `Authorization: Bearer` over `X-Access-Proof`. This order is
  canonical — do not flip it without updating both impls and the client.
- The Rust verification logic (`verify.rs`) is a direct port of the Workers `verify.ts`. Any
  behavioural change in one must be reflected in the other; run conformance vectors to verify.
- `decode_access_proof` normalises `consumeDigest` (camelCase from JS) to `consume_digest`
  (snake_case for serde) before deserialising. If the JSON key ever changes on the client side,
  this must change too.

## What NOT to do

- Do not use `tokio::sync::Mutex` where `std::sync::Mutex` suffices — the nonce map and rate
  limiter only hold the lock for in-memory ops (no `.await` inside the critical section).
- Do not add per-path routing in the router — the single `fallback` handler keeps the logic
  co-located and mirrors the Workers structure.
- Do not change the Redis key prefix (`nftgate:nonce:`) without a migration plan — live nonces
  in Redis would be orphaned.
- Do not remove the hard entry cap from the in-memory store — unbounded growth under high issue
  rates is a DoS vector.
