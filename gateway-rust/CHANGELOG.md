# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.0.1] - 2026-08-27

### Added

- Initial release: generic NFT-gated reverse proxy (Rust / Axum / Tokio)
- `GET /v1/challenge` — issues time-bound nonces (in-memory store; optional Redis for scale-out)
- Proof verification: ed25519, secp256k1, secp256r1 Sui personal-message signatures
- On-chain ownership check via Sui JSON-RPC (`getOwnedObjects`)
- Single-use mode: binds nonce to on-chain `AccessConsumedEvent`
- Per-address rate limiting + configurable body cap
- Public-path passthrough (e.g. `/v1/tip-config`)
- Configurable via environment variables (parity with the Cloudflare Workers sibling)
- Distroless multi-stage Dockerfile (`gcr.io/distroless/cc-debian12:nonroot`)
- Shared conformance vectors with `gateway-workers` (`conformance/vectors.json`)
