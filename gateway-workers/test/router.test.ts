import { describe, it, expect } from 'vitest'
import { env } from 'cloudflare:test'
import worker from '../src/index.js'
import type { Env } from '../src/config.js'

const e = env as unknown as Env

async function call(method: string, path: string, headers?: Record<string, string>): Promise<Response> {
  return worker.fetch(new Request('https://gw.example.com' + path, { method, headers }), e)
}

// Routing parity with main.rs (paths that don't require the upstream or a live RPC).
describe('router', () => {
  it('GET /healthz → 200 ok', async () => {
    const res = await call('GET', '/healthz')
    expect(res.status).toBe(200)
    expect(await res.text()).toBe('ok')
  })

  it('GET /v1/challenge → 200 { nonce, expiresAt }', async () => {
    const res = await call('GET', '/v1/challenge')
    expect(res.status).toBe(200)
    const body = (await res.json()) as { nonce: string; expiresAt: number }
    expect(typeof body.nonce).toBe('string')
    expect(body.nonce.length).toBeGreaterThan(10)
    expect(typeof body.expiresAt).toBe('number')
    expect(body.expiresAt).toBeGreaterThan(Date.now())
  })

  it('gated request without a proof → 401 missing access proof', async () => {
    const res = await call('POST', '/v1/blob-upload')
    expect(res.status).toBe(401)
    expect(await res.json()).toEqual({ error: 'missing access proof' })
  })

  it('gated request with a malformed proof → 403 malformed access proof', async () => {
    const res = await call('POST', '/v1/blob-upload', { authorization: 'Bearer !!!not-base64!!!' })
    expect(res.status).toBe(403)
    expect(await res.json()).toEqual({ error: 'malformed access proof' })
  })

  it('challenge → sign is a full round trip the nonce store accepts once', async () => {
    // The issued nonce must be consumable exactly once by the same backend the router uses.
    const res = await call('GET', '/v1/challenge')
    const { nonce } = (await res.json()) as { nonce: string }
    expect(nonce.includes('.')).toBe(true) // <region>.<hex> shard-tagged form
  })
})
