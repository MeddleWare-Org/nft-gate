/**
 * The pluggable state backend — the Workers analog of the Rust gateway's `NonceStore` enum
 * (in-memory | Redis). It holds the single-use nonce store AND the per-address rate-limit
 * windows, because both need the same per-key atomicity a stateless isolate cannot give.
 *
 * Two implementations:
 * - `DurableObjectBackend` (default): region-sharded, SQLite-backed Durable Objects. Strongly
 *   consistent, atomic single-use consume (the Redis-`GETDEL` analog). Free-tier eligible.
 * - `KvBackend`: Workers KV + best-effort rate limit. Eventually consistent (documented weaker
 *   cross-region replay window in the `SINGLE_USE=false` ownership mode).
 */

export interface NonceBackend {
  /**
   * Issue a fresh, time-bound nonce. `region` selects the DO shard (ignored by KV, which is
   * global). Returns the opaque `<region>.<hex>` nonce and its unix-ms expiry.
   */
  issue(region: string, ttlSecs: number): Promise<{ nonce: string; expiresAt: number }>
  /** Consume a nonce exactly once; true iff it was valid, unexpired, and unused. */
  takeIfValid(nonce: string): Promise<boolean>
  /** Fixed 60s window per verified address. `maxPerMin === 0` disables limiting. */
  rateCheck(address: string, maxPerMin: number, region: string): Promise<boolean>
}

/**
 * Parse the shard tag embedded in a `<region>.<hex>` nonce so a consume call always routes
 * back to the shard that issued it, even under anycast region drift.
 *
 * @param nonce - A nonce in `<region>.<hex>` format.
 * @returns The region prefix (e.g. `"eu"`), or `"g"` if no dot is present.
 */
export function shardOfNonce(nonce: string): string {
  const dot = nonce.indexOf('.')
  return dot > 0 ? nonce.slice(0, dot) : 'g'
}

/**
 * Generate a cryptographically random 48-character hex string (24 bytes of entropy). Matches
 * the Rust gateway's `random_nonce()` entropy so conformance vectors apply to both.
 *
 * @returns A 48-character lowercase hex string.
 */
export function randomHex24(): string {
  const buf = new Uint8Array(24)
  crypto.getRandomValues(buf)
  let out = ''
  for (let i = 0; i < buf.length; i++) out += buf[i].toString(16).padStart(2, '0')
  return out
}
