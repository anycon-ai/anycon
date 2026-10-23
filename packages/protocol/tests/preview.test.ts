import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import {
  ProtocolError,
  decodePreviewLaunch,
  encodePreviewLaunch,
  isValidPreviewLabel,
  previewOrigin,
  type PreviewLaunch,
} from '../src/index.js'

const launch: PreviewLaunch = {
  v: 1,
  node: '0123456789abcdef0123456789abcdef',
  node_public: 'ab'.repeat(32),
  label: 'abcdefghijklmnopqrstuvwxyz',
  port: 5173,
  ticket_id: 'fedcba9876543210fedcba9876543210',
  psk: 'cd'.repeat(32),
  token: 'pvt_token',
  expires_at_ms: 1_800_000_000_000,
  path: '/app?x=中文',
}

function withPatch(patch: Record<string, unknown>): string {
  return encodePreviewLaunch({ ...launch, ...patch } as PreviewLaunch)
}

describe('preview launch', () => {
  it('roundtrips, is url-safe, accepts a leading #', () => {
    const frag = encodePreviewLaunch(launch)
    assert.match(frag, /^[A-Za-z0-9_-]+$/)
    assert.deepEqual(decodePreviewLaunch(frag), launch)
    assert.deepEqual(decodePreviewLaunch(`#${frag}`), launch)
  })

  it('rejects malformed launches', () => {
    for (const frag of [
      'not base64!',
      withPatch({ v: 2 }),
      withPatch({ label: 'ABC' }),
      withPatch({ port: 0 }),
      withPatch({ port: 70000 }),
      withPatch({ psk: 'xx' }),
      withPatch({ node: 'short' }),
      withPatch({ token: '' }),
      withPatch({ path: 'relative' }),
      withPatch({ path: '//evil.example/' }),
    ]) {
      assert.throws(() => decodePreviewLaunch(frag), ProtocolError)
    }
  })
})

describe('preview origin', () => {
  it('validates labels', () => {
    assert.equal(isValidPreviewLabel('abcdefghijklmnopqrstuvwxyz'), true)
    assert.equal(isValidPreviewLabel('abcdefghijklmnopqrstuvwxy1'), false)
    assert.equal(isValidPreviewLabel('abc'), false)
  })

  it('fills the template', () => {
    assert.equal(
      previewOrigin('https://{label}.aiplace.app', 'abcdefghijklmnopqrstuvwxyz'),
      'https://abcdefghijklmnopqrstuvwxyz.aiplace.app',
    )
    assert.throws(() => previewOrigin('https://{label}.aiplace.app', 'evil.com/'), ProtocolError)
    assert.throws(
      () => previewOrigin('https://aiplace.app', 'abcdefghijklmnopqrstuvwxyz'),
      ProtocolError,
    )
  })
})
