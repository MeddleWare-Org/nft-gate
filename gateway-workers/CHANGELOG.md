# Changelog

All notable changes to `@meddleware/nft-gate-gateway` are documented here.

## [0.0.1] - 2026-08-27

### Added
- `UPSTREAM_AUTH_HEADERS` env var: comma-separated `Name: value` header pairs injected on every
  upstream relay request. Enables Cloudflare Access service-token authentication when the relay
  origin is CF-Access-locked. Set via `wrangler secret put UPSTREAM_AUTH_HEADERS` — never in
  `wrangler.toml`.
- `wrangler.toml` operator placeholders for `UPSTREAM_URL`, route pattern, and zone name.
- `test/config.test.ts`: unit tests for `UPSTREAM_AUTH_HEADERS` parsing (single entry, two CF
  Access entries, whitespace trimming, value-internal colons).
