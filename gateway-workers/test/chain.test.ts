import { describe, it, expect } from 'vitest'
import { SuiRpc, nonceMatches, isConsumedEvent, eventMatches } from '../src/chain.js'
import { bytesToBase64 } from '../src/crypto.js'

// Pure match/parse helpers — mirror sui_rpc.rs unit tests.
describe('chain helpers', () => {
  it('nonceMatches byte array', () => {
    expect(nonceMatches([110, 111, 110, 99, 101], 'nonce')).toBe(true)
    expect(nonceMatches([1, 2, 3], 'nonce')).toBe(false)
  })

  it('nonceMatches base64', () => {
    const b64 = bytesToBase64(new TextEncoder().encode('nonce'))
    expect(nonceMatches(b64, 'nonce')).toBe(true)
  })

  it('packageOf extracts the prefix', () => {
    expect(SuiRpc.packageOf('0xabc::access_gate::AccessNFT')).toBe('0xabc')
  })

  it('cacheKey is stable', () => {
    expect(SuiRpc.cacheKey('0xa', '0xt', '0xg')).toBe('0xa|0xt|0xg')
    expect(SuiRpc.cacheKey('0xa', '0xt', undefined)).toBe('0xa|0xt|-')
  })

  it('eventMatches checks sender, nonce, and gate', () => {
    const ev = {
      type: '0xabc::access_gate::AccessConsumedEvent',
      sender: '0xowner',
      parsedJson: { nonce: [110, 111, 110, 99, 101], gate_id: '0xgate' },
    }
    expect(isConsumedEvent(ev)).toBe(true)
    expect(eventMatches(ev, 'nonce', '0xowner', '0xgate')).toBe(true)
    expect(eventMatches(ev, 'nonce', '0xattacker', '0xgate')).toBe(false) // wrong sender
    expect(eventMatches(ev, 'other', '0xowner', '0xgate')).toBe(false) // wrong nonce
    expect(eventMatches(ev, 'nonce', '0xowner', '0xwrong')).toBe(false) // wrong gate
    expect(eventMatches(ev, 'nonce', '0xowner', undefined)).toBe(true) // gate unconstrained
  })
})
