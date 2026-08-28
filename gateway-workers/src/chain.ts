/**
 * Production {@link ChainQuery} over Sui JSON-RPC (`suix_getOwnedObjects`, `suix_queryEvents`,
 * `sui_getTransactionBlock`) via `fetch`. A 1:1 port of the Rust gateway's `sui_rpc.rs`,
 * including the identical request JSON shapes. The pure match/parse helpers are exported for
 * unit tests; the RPC round-trips are covered by the localnet/integration loop.
 */

import type { ChainQuery } from './verify.js'
import { base64ToBytes } from './crypto.js'

type Json = unknown

/**
 * A cached result of a single ownership query.
 * `owns` — whether the address held the NFT at query time.
 * `expiry` — unix-ms timestamp after which this entry must be discarded.
 */
interface CacheEntry {
  owns: boolean
  expiry: number
}

export class SuiRpc implements ChainQuery {
  private readonly cache = new Map<string, CacheEntry>()

  /**
   * @param rpcUrl - Sui JSON-RPC endpoint URL.
   * @param cacheTtlMs - Ownership-cache TTL in ms. 0 disables the cache (live check every request).
   * @param authHeader - Optional header injected on every RPC call (e.g. credentialed fullnode auth).
   */
  constructor(
    private readonly rpcUrl: string,
    /** Ownership-cache TTL (ms). 0 = disabled (every gated check is live on-chain). */
    private readonly cacheTtlMs: number = 0,
    private readonly authHeader?: { name: string; value: string },
  ) {}

  /**
   * Send a Sui JSON-RPC 2.0 request and return the `result` field.
   *
   * @param method - JSON-RPC method name (e.g. `suix_getOwnedObjects`).
   * @param params - Positional parameters array.
   * @returns The `result` value from the RPC response, or `null`.
   * @throws If the HTTP response is not OK or the RPC body contains an `error` field.
   */
  private async call(method: string, params: Json): Promise<Json> {
    const headers: Record<string, string> = { 'content-type': 'application/json' }
    if (this.authHeader) headers[this.authHeader.name] = this.authHeader.value
    const resp = await fetch(this.rpcUrl, {
      method: 'POST',
      headers,
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
    })
    if (!resp.ok) throw new Error(`rpc http ${resp.status} from ${method}`)
    const json = (await resp.json()) as Record<string, Json>
    if (json && typeof json === 'object' && 'error' in json && json.error) {
      throw new Error(`rpc error from ${method}: ${JSON.stringify(json.error)}`)
    }
    return (json as { result?: Json }).result ?? null
  }

  /**
   * Extract the package address from a `<pkg>::module::Type` string.
   *
   * @param nftType - Fully-qualified Move type string.
   * @returns The package address, or `undefined` if the string cannot be parsed.
   */
  static packageOf(nftType: string): string | undefined {
    const head = nftType.split('::')[0]
    return head && head.length > 0 ? head : undefined
  }

  /**
   * Build a stable cache key from the three ownership-query parameters.
   *
   * @param address - Sui address.
   * @param nftType - Fully-qualified NFT type string.
   * @param gateId - Optional gate object ID constraint.
   * @returns A pipe-delimited string suitable for use as a `Map` key.
   */
  static cacheKey(address: string, nftType: string, gateId?: string): string {
    return `${address}|${nftType}|${gateId ?? '-'}`
  }

  /**
   * Uncached, live `suix_getOwnedObjects` ownership query.
   *
   * @param address - Sui address to query.
   * @param nftType - NFT struct type to filter by.
   * @param gateId - If given, only count objects whose `gate_id` field matches.
   * @returns `true` if at least one qualifying NFT is owned.
   */
  private async ownsNftLive(address: string, nftType: string, gateId?: string): Promise<boolean> {
    const params = [
      address,
      { filter: { StructType: nftType }, options: { showContent: true } },
      null,
      50,
    ]
    const result = (await this.call('suix_getOwnedObjects', params)) as { data?: Json[] } | null
    const data = (result && Array.isArray(result.data) ? result.data : []) as Json[]
    if (gateId === undefined) return data.length > 0
    return data.some((entry) => pointerStr(entry, ['data', 'content', 'fields', 'data', 'fields', 'gate_id']) === gateId)
  }

  /**
   * Check whether `address` owns at least one NFT of `nftType`. Uses the in-process ownership
   * cache when `cacheTtlMs > 0`; otherwise every call is live on-chain.
   *
   * @param address - Sui address to check.
   * @param nftType - Fully-qualified NFT type string.
   * @param gateId - Optional gate object ID constraint.
   * @returns `true` if the address owns a qualifying NFT.
   */
  async ownsNft(address: string, nftType: string, gateId?: string): Promise<boolean> {
    // Cache is OFF by default (ttl 0) so a gated action is confirmed live on-chain.
    if (this.cacheTtlMs > 0) {
      const key = SuiRpc.cacheKey(address, nftType, gateId)
      const hit = this.cache.get(key)
      const now = Date.now()
      if (hit && hit.expiry > now) return hit.owns
      const owns = await this.ownsNftLive(address, nftType, gateId)
      this.cache.set(key, { owns, expiry: now + this.cacheTtlMs })
      return owns
    }
    return this.ownsNftLive(address, nftType, gateId)
  }

  /**
   * Directly verify the consume transaction named by `digest` via `sui_getTransactionBlock`.
   * The tx must have succeeded and emitted a matching `AccessConsumedEvent`. Defence-in-depth
   * atop the sender+nonce event query.
   *
   * @param digest - Transaction digest of the on-chain consume.
   * @param nonce - The challenge nonce that was consumed.
   * @param address - Expected transaction sender.
   * @param gateId - Optional gate object ID constraint.
   * @returns `true` if the transaction confirms the consume.
   */
  private async consumeTxMatches(
    digest: string,
    nonce: string,
    address: string,
    gateId?: string,
  ): Promise<boolean> {
    const params = [digest, { showEvents: true, showEffects: true }]
    const result = (await this.call('sui_getTransactionBlock', params)) as Json
    const status = pointerStr(result, ['effects', 'status', 'status'])
    if (status !== 'success') return false
    const events = (pointer(result, ['events']) as Json[] | undefined) ?? []
    return (
      Array.isArray(events) &&
      events.some((ev) => isConsumedEvent(ev) && eventMatches(ev, nonce, address, gateId))
    )
  }

  /**
   * Verify a single-use consume via `suix_queryEvents`. Queries for an `AccessConsumedEvent`
   * emitted by `address` carrying `nonce`; if `consumeDigest` is also provided, additionally
   * verifies that exact transaction (F4: defence in depth).
   *
   * @param nonce - The challenge nonce that was consumed.
   * @param address - The Sui address that submitted the consume transaction.
   * @param nftType - Fully-qualified NFT type used to derive the event package.
   * @param gateId - Optional gate object ID constraint.
   * @param consumeDigest - Optional transaction digest for direct tx verification.
   * @returns `true` if a matching consume event (and, when given, a matching tx) is found.
   */
  async consumeEventMatches(
    nonce: string,
    address: string,
    nftType: string,
    gateId?: string,
    consumeDigest?: string,
  ): Promise<boolean> {
    const pkg = SuiRpc.packageOf(nftType)
    if (!pkg) throw new Error('cannot derive package from nft_type')
    const eventType = `${pkg}::access_gate::AccessConsumedEvent`
    // Filter by BOTH the event type AND the emitting Sender (F5): O(this user's events).
    const params = [{ All: [{ MoveEventType: eventType }, { Sender: address }] }, null, 50, true]
    const result = (await this.call('suix_queryEvents', params)) as { data?: Json[] } | null
    const data = (result && Array.isArray(result.data) ? result.data : []) as Json[]
    const primary = data.some((ev) => eventMatches(ev, nonce, address, gateId))
    if (!primary) return false
    // F4: if a consume tx digest was supplied, verify that exact transaction too.
    if (consumeDigest !== undefined) {
      return this.consumeTxMatches(consumeDigest, nonce, address, gateId)
    }
    return true
  }
}

// ── pure helpers (unit-tested; mirror sui_rpc.rs) ────────────────────────────

/**
 * Traverse a nested JSON value by a sequence of object keys.
 *
 * @param v - The root JSON value.
 * @param path - Sequence of object keys to follow.
 * @returns The value at the path, or `undefined` if any step is missing or non-object.
 */
function pointer(v: Json, path: string[]): Json {
  let cur: Json = v
  for (const key of path) {
    if (cur && typeof cur === 'object' && !Array.isArray(cur) && key in (cur as Record<string, Json>)) {
      cur = (cur as Record<string, Json>)[key]
    } else {
      return undefined
    }
  }
  return cur
}

/**
 * Like {@link pointer} but returns `undefined` if the resolved value is not a string.
 *
 * @param v - The root JSON value.
 * @param path - Sequence of object keys to follow.
 * @returns The string value at the path, or `undefined`.
 */
function pointerStr(v: Json, path: string[]): string | undefined {
  const r = pointer(v, path)
  return typeof r === 'string' ? r : undefined
}

/** True if the event's type ends with `::access_gate::AccessConsumedEvent`. */
export function isConsumedEvent(ev: Json): boolean {
  const t = pointerStr(ev, ['type'])
  return t !== undefined && t.endsWith('::access_gate::AccessConsumedEvent')
}

/**
 * Match the on-chain `AccessConsumedEvent.nonce` (`vector<u8>`), rendered by RPC as either an
 * array of byte numbers or a base64 string, against the challenge nonce's UTF-8 bytes.
 */
export function nonceMatches(eventNonce: Json, nonce: string): boolean {
  const want = new TextEncoder().encode(nonce)
  if (Array.isArray(eventNonce)) {
    if (eventNonce.length !== want.length) return false
    return eventNonce.every((b, i) => typeof b === 'number' && b === want[i])
  }
  if (typeof eventNonce === 'string') {
    try {
      const decoded = base64ToBytes(eventNonce)
      return decoded.length === want.length && decoded.every((b, i) => b === want[i])
    } catch {
      return false
    }
  }
  return false
}

/** sender == address, nonce matches, and (if given) gate_id matches. */
export function eventMatches(ev: Json, nonce: string, address: string, gateId?: string): boolean {
  const senderOk = pointerStr(ev, ['sender']) === address
  const nonceVal = pointer(ev, ['parsedJson', 'nonce'])
  const nonceOk = nonceVal !== undefined && nonceMatches(nonceVal, nonce)
  const gateOk = gateId === undefined ? true : pointerStr(ev, ['parsedJson', 'gate_id']) === gateId
  return senderOk && nonceOk && gateOk
}
