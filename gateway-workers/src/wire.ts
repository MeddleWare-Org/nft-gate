/**
 * The shared wire-format helpers, REUSED from `@meddleware/nft-gate-client` rather than
 * re-implemented — there must be exactly one source for `personalMessageForNonce` and the
 * proof token shape across the toolkit (the "one contract that spans all three" rule in
 * nft-gate/CLAUDE.md).
 *
 * Imported by relative path (the in-repo convention) directly from the client's `proof.ts` /
 * `types.ts`, which import only `./types.js` — so this pulls in NO `@mysten/sui` and keeps the
 * Worker bundle lean.
 *
 * Self-contained-mirror alternative (documented for future review): if the Worker must ship
 * fully decoupled from the client package, replace these two lines with a local copy of
 * `personalMessageForNonce` + `decodeAccessProof` guarded by `conformance/vectors.json`.
 */

export {
  personalMessageForNonce,
  decodeAccessProof,
} from '@meddleware/nft-gate-client'
export type { AccessProof } from '@meddleware/nft-gate-client'
