import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import { decode, encode } from '@msgpack/msgpack'

import {
  ENVELOPE_VERSION,
  ProtocolError,
  UnknownControlError,
  controlBody,
  controlEnvelope,
  decodeControl,
  decodeEnrollFrame,
  decodeEnvelope,
  encodeControl,
  encodeEnrollFrame,
  encodeEnvelope,
  makeEnvelope,
  type EnrollFrame,
  type Envelope,
} from '../src/index.js'

const A = '0123456789abcdef0123456789abcdef'
const B = 'fedcba9876543210fedcba9876543210'

function env(body: Uint8Array, seq = 1): Envelope {
  return makeEnvelope(A, B, 'data', seq, body)
}

describe('envelope', () => {
  it('往返，body 保持 Uint8Array', () => {
    const e = env(Uint8Array.of(1, 2, 3), 7)
    const back = decodeEnvelope(encodeEnvelope(e))
    assert.deepEqual(back, e)
    assert.ok(back.body instanceof Uint8Array)
    assert.equal(e.v, ENVELOPE_VERSION)
  })

  it('body 用 bin 而不是 array：bin8 / bin16 / bin32 头字节', () => {
    const header = (len: number) => {
      const bytes = encodeEnvelope(env(new Uint8Array(len)))
      // 找到 body 键之后的类型字节：键 "body" = a4 62 6f 64
      const at = bytes.findIndex(
        (_, i) => bytes[i] === 0xa4 && bytes[i + 1] === 0x62 && bytes[i + 2] === 0x6f,
      )
      return bytes[at + 5]
    }
    assert.equal(header(0), 0xc4)
    assert.equal(header(255), 0xc4)
    assert.equal(header(256), 0xc5)
    assert.equal(header(65535), 0xc5)
    assert.equal(header(65536), 0xc6)
    assert.equal(decodeEnvelope(encodeEnvelope(env(new Uint8Array(70_000)))).body.length, 70_000)
  })

  it('键序固定为 v, from, to, kind, seq, body', () => {
    const e = env(Uint8Array.of(9))
    const shuffled = { body: e.body, seq: e.seq, kind: e.kind, to: e.to, from: e.from, v: e.v }
    assert.deepEqual(Object.keys(decode(encodeEnvelope(shuffled)) as object), [
      'v',
      'from',
      'to',
      'kind',
      'seq',
      'body',
    ])
  })

  it('解码出的 body 不是输入 buffer 的视图', () => {
    const bytes = encodeEnvelope(env(Uint8Array.of(1, 2, 3)))
    const back = decodeEnvelope(bytes)
    bytes.fill(0)
    assert.deepEqual([...back.body], [1, 2, 3])
  })

  it('拒绝非法输入', () => {
    assert.throws(() => decodeEnvelope(Uint8Array.of(0xc1)), ProtocolError)
    assert.throws(() => decodeEnvelope(encode([1, 2, 3])), /map/)
    assert.throws(
      () =>
        decodeEnvelope(
          encode({ v: 1, from: A, to: B, kind: 'bogus', seq: 1, body: new Uint8Array() }),
        ),
      /kind/,
    )
    assert.throws(
      () => decodeEnvelope(encode({ v: 1, from: A, to: B, kind: 'data', seq: 1, body: [1, 2] })),
      /bin/,
    )
    assert.throws(() => encodeEnvelope({ ...env(new Uint8Array()), seq: -1 }), ProtocolError)
    assert.throws(() => encodeEnvelope({ ...env(new Uint8Array()), seq: 1.5 }), ProtocolError)
  })
})

describe('control', () => {
  it('rotate_token 是只有 type 的 map；pair_begin / pair_join 带 match_id（type 在前）', () => {
    assert.deepEqual(decode(encodeControl({ type: 'rotate_token' })), { type: 'rotate_token' })
    const begin = decode(encodeControl({ type: 'pair_begin', match_id: '0123456789abcdef' }))
    assert.deepEqual(begin, { type: 'pair_begin', match_id: '0123456789abcdef' })
    assert.deepEqual(Object.keys(begin as object), ['type', 'match_id'])
    const dropped = decode(encodeControl({ type: 'dropped', reason: 'too_large', seq: 9 }))
    assert.deepEqual(Object.keys(dropped as object), ['type', 'seq', 'reason'])
  })

  it('auth 不带 device_token 时整键省略，带 origin 的 client 保留', () => {
    const bytes = encodeControl({
      type: 'auth',
      protocol_version: 1,
      client: { kind: 'web', name: 'n', platform: 'browser', version: '1', origin: 'https://h' },
    })
    const decoded = decode(bytes) as Record<string, unknown>
    assert.ok(!('device_token' in decoded))
    assert.deepEqual(Object.keys(decoded), ['type', 'protocol_version', 'client'])
    assert.deepEqual(Object.keys(decoded.client as object), [
      'name',
      'kind',
      'platform',
      'version',
      'origin',
    ])
  })

  it('不省略 nil 的字段（Quota 的 *_max）保持 null', () => {
    const bytes = encodeControl({
      type: 'quota_update',
      quota: {
        devices: 1,
        devices_max: null,
        daemons: 0,
        daemons_max: null,
        connections: 2,
        connections_max: 3,
      },
    })
    const decoded = decode(bytes) as { quota: Record<string, unknown> }
    assert.equal(decoded.quota.devices_max, null)
    assert.equal(decoded.quota.connections_max, 3)
  })

  it('未知 type 抛 UnknownControlError，调用方可忽略这一帧', () => {
    assert.throws(
      () => decodeControl(encode({ type: 'from_the_future', x: 1 })),
      UnknownControlError,
    )
    assert.throws(() => decodeControl(encode({ nope: 1 })), ProtocolError)
  })

  it('controlEnvelope / controlBody 往返', () => {
    const e = controlEnvelope(A, 5, { type: 'pair_join', match_id: '0123456789abcdef' })
    assert.equal(e.kind, 'control')
    assert.equal(e.to, '')
    assert.deepEqual(controlBody(decodeEnvelope(encodeEnvelope(e))), {
      type: 'pair_join',
      match_id: '0123456789abcdef',
    })
    assert.throws(() => controlBody(env(new Uint8Array())), ProtocolError)
  })
})

describe('enroll frames (ADR-36)', () => {
  it('roundtrips every variant and omits empty reply fields', () => {
    const frames: EnrollFrame[] = [
      {
        type: 'hello',
        v: 1,
        method: 'code',
        approval: true,
        nonce: '00'.repeat(16),
        hint: { platform: 'browser', kind: 'web', name: 'Chrome' },
      },
      { type: 'reply', status: 'wait_approval', nonce: 'ab'.repeat(16) },
      { type: 'reply', status: 'rejected', reason: 'backoff', retry_after_ms: 60000 },
      { type: 'cpace', share: 'cd'.repeat(32) },
      { type: 'noise', msg: 'ef'.repeat(48) },
    ]
    for (const f of frames) assert.deepEqual(decodeEnrollFrame(encodeEnrollFrame(f)), f)
    const keys = Object.keys(decode(encodeEnrollFrame(frames[2]!)) as object)
    assert.deepEqual(keys, ['type', 'status', 'reason', 'retry_after_ms'])
    const hello = decode(encodeEnrollFrame(frames[0]!)) as { hint: object }
    assert.deepEqual(Object.keys(hello.hint), ['name', 'kind', 'platform'])
  })

  it('rejects unknown frame types', () => {
    assert.throws(() => decodeEnrollFrame(encode({ type: 'nope' })), /unknown enroll frame type/)
    assert.throws(() => decodeEnrollFrame(encode([1])), /must be a map/)
  })
})
