/**
 * Region-sharded, SQLite-backed Durable Object state — the default nonce store + rate limiter.
 *
 * Each region gets its own DO instance (id = `nonce:<region>`), which Cloudflare places near
 * the request that first initializes it — so the shard sits close to its users. The issued
 * nonce embeds the region tag (`<region>.<hex>`) so a later consume always routes back to the
 * ISSUING shard, even under anycast drift.
 *
 * Single-threaded per DO ⇒ `takeIfValid` is atomic (the Redis-`GETDEL` analog): a nonce is
 * consumed exactly once. Free-tier eligible (SQLite storage on the Workers Free plan).
 */

import { DurableObject } from 'cloudflare:workers'
import type { NonceBackend } from './types.js'
import { randomHex24, shardOfNonce } from './types.js'

/** SQLite row shape for the `nonces` table. */
interface NonceRow {
  expiry: number
}
/** SQLite row shape for the `rate` table. */
interface RateRow {
  start: number
  count: number
}
/** Result row for `SELECT COUNT(*) AS c`. */
interface CountRow {
  c: number
}

/**
 * Durable Object that provides the SQLite-backed nonce store and per-address rate limiter.
 * Each instance corresponds to one region shard (id = `nonce:<region>`). Single-threaded
 * per instance, so `takeIfValid` is naturally atomic (the Redis-`GETDEL` analog).
 */
export class NonceRateState extends DurableObject {
  private readonly sql: SqlStorage

  /**
   * @param ctx - Durable Object state, provides the SQLite storage handle.
   * @param env - Worker environment (not used directly; passed through to the base class).
   */
  constructor(ctx: DurableObjectState, env: unknown) {
    super(ctx as DurableObjectState, env as never)
    this.sql = ctx.storage.sql
    this.sql.exec(
      'CREATE TABLE IF NOT EXISTS nonces (nonce TEXT PRIMARY KEY, expiry INTEGER NOT NULL)',
    )
    this.sql.exec(
      'CREATE TABLE IF NOT EXISTS rate (addr TEXT PRIMARY KEY, start INTEGER NOT NULL, count INTEGER NOT NULL)',
    )
  }

  /** Store a fresh nonce with a hard entry cap (evict soonest-to-expire). Returns expiry ms. */
  issue(nonce: string, ttlSecs: number, maxEntries: number): number {
    const now = Date.now()
    const expiry = now + ttlSecs * 1000
    // Prune expired first (bounded growth independent of issue cadence).
    this.sql.exec('DELETE FROM nonces WHERE expiry <= ?', now)
    const count = (this.sql.exec('SELECT COUNT(*) AS c FROM nonces').one() as unknown as CountRow).c
    if (count >= Math.max(1, maxEntries)) {
      this.sql.exec(
        'DELETE FROM nonces WHERE nonce = (SELECT nonce FROM nonces ORDER BY expiry ASC LIMIT 1)',
      )
    }
    this.sql.exec('INSERT OR REPLACE INTO nonces (nonce, expiry) VALUES (?, ?)', nonce, expiry)
    return expiry
  }

  /** Atomically consume a nonce: true iff present and unexpired. Deletes on read (single-use). */
  takeIfValid(nonce: string): boolean {
    const now = Date.now()
    const rows = this.sql.exec('SELECT expiry FROM nonces WHERE nonce = ?', nonce).toArray() as unknown as NonceRow[]
    if (rows.length === 0) return false
    // Consume unconditionally (present ⇒ gone), so it can never be replayed.
    this.sql.exec('DELETE FROM nonces WHERE nonce = ?', nonce)
    return Number(rows[0].expiry) > now
  }

  /** Fixed 60s window per address. */
  rateCheck(addr: string, maxPerMin: number): boolean {
    if (maxPerMin === 0) return true
    const now = Date.now()
    const rows = this.sql.exec('SELECT start, count FROM rate WHERE addr = ?', addr).toArray() as unknown as RateRow[]
    let start = now
    let count = 0
    if (rows.length > 0) {
      const r = rows[0]
      if (now - Number(r.start) < 60000) {
        start = Number(r.start)
        count = Number(r.count)
      }
    }
    if (count >= maxPerMin) return false
    this.sql.exec('INSERT OR REPLACE INTO rate (addr, start, count) VALUES (?, ?, ?)', addr, start, count + 1)
    return true
  }
}

/** {@link NonceBackend} that fans out to per-region {@link NonceRateState} DO shards. */
export class DurableObjectBackend implements NonceBackend {
  /**
   * @param ns - The Durable Object namespace binding for `NonceRateState`.
   * @param shardMode - `"region"` places each shard near its users; `"global"` uses one instance.
   * @param maxEntries - Hard cap on live nonce entries per shard (evicts oldest when reached).
   */
  constructor(
    private readonly ns: DurableObjectNamespace<NonceRateState>,
    private readonly shardMode: 'region' | 'global',
    private readonly maxEntries: number,
  ) {}

  /**
   * Retrieve the DO stub for the given region, creating a new shard if needed.
   *
   * @param region - The region tag used to derive the DO instance name (`nonce:<region>`).
   * @returns A `DurableObjectStub` for that region's shard.
   */
  private stub(region: string): DurableObjectStub<NonceRateState> {
    const id = this.ns.idFromName('nonce:' + region)
    return this.ns.get(id)
  }

  async issue(region: string, ttlSecs: number): Promise<{ nonce: string; expiresAt: number }> {
    const shard = this.shardMode === 'global' ? 'g' : region
    const nonce = `${shard}.${randomHex24()}`
    const expiresAt = await this.stub(shard).issue(nonce, ttlSecs, this.maxEntries)
    return { nonce, expiresAt }
  }

  async takeIfValid(nonce: string): Promise<boolean> {
    return this.stub(shardOfNonce(nonce)).takeIfValid(nonce)
  }

  async rateCheck(address: string, maxPerMin: number, region: string): Promise<boolean> {
    const shard = this.shardMode === 'global' ? 'g' : region
    return this.stub(shard).rateCheck(address, maxPerMin)
  }
}
