/**
 * Backend selection — the Workers analog of the Rust gateway's `main.rs` store bootstrap
 * (`REDIS_URL ? redis : in_memory`). Chooses the Durable Object or KV backend from config +
 * available bindings, failing fast if the chosen backend's binding is missing.
 */

import type { Config, Env } from '../config.js'
import type { NonceBackend } from './types.js'
import type { NonceRateState } from './durable_object.js'
import { DurableObjectBackend } from './durable_object.js'
import { KvBackend } from './kv.js'

/**
 * Instantiate the appropriate {@link NonceBackend} from config and available bindings.
 * Mirrors the Rust gateway's startup-time store selection (`REDIS_URL ? redis : in_memory`).
 *
 * @param cfg - The resolved gateway config, potentially modified by the quota-degrade flag.
 * @param env - Worker environment bindings.
 * @returns A configured `NonceBackend` instance.
 * @throws If the required binding for the chosen backend is absent.
 */
export function makeBackend(cfg: Config, env: Env): NonceBackend {
  if (cfg.nonceBackend === 'kv') {
    if (!env.NONCE_KV) throw new Error('NONCE_BACKEND=kv but the NONCE_KV binding is missing')
    return new KvBackend(env.NONCE_KV)
  }
  if (!env.NONCE_STATE) {
    throw new Error('NONCE_BACKEND=durable-object but the NONCE_STATE binding is missing')
  }
  return new DurableObjectBackend(
    env.NONCE_STATE as DurableObjectNamespace<NonceRateState>,
    cfg.nonceShard,
    cfg.nonceMaxEntries,
  )
}
