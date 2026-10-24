import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { describe, it } from 'node:test'

import {
  PREVIEW_LABEL_ALPHABET,
  ProtocolError,
  TUNNEL_ACK_DELAY_MS,
  TUNNEL_ACK_EVERY_BYTES,
  TUNNEL_ACK_EVERY_MESSAGES,
  TUNNEL_CLIENT_INFLIGHT,
  TUNNEL_DAEMON_INFLIGHT,
  TUNNEL_FRAME_HEADER,
  TUNNEL_HANDSHAKE_ACCEPT,
  TUNNEL_HANDSHAKE_REJECT,
  TUNNEL_HANDSHAKE_VERSION,
  TUNNEL_MAX_DATA_FRAME,
  TUNNEL_MAX_HEAD,
  TUNNEL_MAX_MESSAGE,
  TUNNEL_MAX_STREAMS,
  TUNNEL_MAX_WS_MESSAGE,
  TUNNEL_PROLOGUE_PREFIX,
  TUNNEL_PSK_LEN,
  TUNNEL_STREAM_WINDOW,
  TUNNEL_TICKET_ID_LEN,
  TUNNEL_WS_BINARY,
  TUNNEL_WS_CLOSE_REASON_MAX,
  TUNNEL_WS_FIN,
  TunnelError,
  TunnelFrameType,
  decodeGoaway,
  decodeMessage,
  decodePreviewLaunch,
  decodeRequestHead,
  decodeReset,
  decodeResponseHead,
  encodeGoaway,
  encodeMessage,
  encodePreviewLaunch,
  encodeRequestHead,
  encodeReset,
  encodeResponseHead,
  handshakeAccept,
  handshakeInit,
  handshakeReject,
  isValidPreviewLabel,
  labelFromBytes,
  messageLen,
  newTunnelFrame,
  parseHandshakeInit,
  parseHandshakeReply,
  parseWindow,
  parseWsClose,
  parseWsMessage,
  previewOrigin,
  previewPrologue,
  windowPayload,
  wsClose,
  wsMessage,
  type PreviewLaunch,
  type TunnelErrorCode,
  type TunnelFrame,
} from '../src/index.js'
import { fromHex, repoRoot, toHex } from './helpers.js'

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
      // 浏览器把 `\` 当 `/`，并且去掉 tab / 换行：这些写法最后都会变成协议相对的 URL
      withPatch({ path: '/\\evil.example/' }),
      withPatch({ path: '/app\\x' }),
      withPatch({ path: '/\tevil.example' }),
      withPatch({ path: '/\n/evil.example' }),
      withPatch({ path: '/\r/evil.example' }),
      withPatch({ path: '/a\u0000b' }),
      withPatch({ path: '/a\u001fb' }),
      withPatch({ path: '/a\u007fb' }),
    ]) {
      assert.throws(() => decodePreviewLaunch(frag), ProtocolError)
    }
  })
})

describe('preview launch path', () => {
  it('keeps ordinary paths, queries and non-ASCII text', () => {
    for (const path of [
      '/',
      '/a/b?c=d&e=f',
      '/中文/路径?q=é',
      '/a%5Cb',
      '/a%0Ab',
      "/it's",
      '/a b',
      '/#frag',
    ]) {
      assert.equal(decodePreviewLaunch(withPatch({ path })).path, path)
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

// ---------------------------------------------------------------------------
// 隧道：Rust 生成的字节向量（tests/vectors/preview.json，`tools/gen-vectors`）
// ---------------------------------------------------------------------------

interface VectorFrame {
  type: string
  type_byte: number
  stream: number
  payload_hex: string
}

type Vector = { name: string; kind: string; bytes_hex: string } & Record<string, unknown>

const vectors = JSON.parse(
  readFileSync(new URL('./vectors/preview.json', import.meta.url), 'utf8'),
) as Vector[]

function ofKind(kind: string): Vector[] {
  const found = vectors.filter((v) => v.kind === kind)
  assert.ok(found.length > 0, `preview.json 里没有 ${kind} 向量`)
  return found
}

function framesOf(list: VectorFrame[]): TunnelFrame[] {
  return list.map((f) => ({
    type: f.type_byte as TunnelFrame['type'],
    stream: f.stream,
    payload: fromHex(f.payload_hex),
  }))
}

/** JSON 里的 u64 超过 2^53 会丢精度，所以以字节里的 ack 为准，JSON 值只在安全范围内交叉检查。 */
function ackOf(vec: Vector): bigint {
  const fromBytes = BigInt(`0x${vec.bytes_hex.slice(0, 16)}`)
  const json = vec.ack as number
  if (Number.isSafeInteger(json)) assert.equal(BigInt(json), fromBytes)
  return fromBytes
}

describe('tunnel vectors (preview.json)', () => {
  it('covers every vector kind this suite knows', () => {
    const kinds = new Set(vectors.map((v) => v.kind))
    for (const k of [
      'message',
      'decode',
      'request_head',
      'response_head',
      'reset',
      'goaway',
      'window',
      'ws_message',
      'ws_close',
      'handshake_init',
      'handshake_reply',
      'prologue',
      'label',
    ]) {
      assert.ok(kinds.has(k), `no ${k} vector`)
      kinds.delete(k)
    }
    assert.deepEqual([...kinds], [], 'preview.json has a vector kind this suite does not check')
  })

  for (const vec of ofKind('message')) {
    it(`message ${vec.name}: encodes to the exact bytes and decodes back`, () => {
      const frames = framesOf(vec.frames as VectorFrame[])
      const ack = ackOf(vec)
      assert.equal(toHex(encodeMessage(ack, frames)), vec.bytes_hex)
      assert.equal(messageLen(frames), vec.bytes_hex.length / 2)
      const back = decodeMessage(fromHex(vec.bytes_hex))
      assert.equal(back.ack, ack)
      assert.deepEqual(back.frames, frames)
    })
  }

  for (const vec of ofKind('decode')) {
    it(`decode ${vec.name}`, () => {
      const bytes = fromHex(vec.bytes_hex)
      const outcome = vec.outcome as
        | { ok: true; ack: number; frames: VectorFrame[] }
        | { err: TunnelErrorCode }
      if ('err' in outcome) {
        assert.throws(
          () => decodeMessage(bytes),
          (e: unknown) => e instanceof TunnelError && e.code === outcome.err,
        )
      } else {
        const m = decodeMessage(bytes)
        assert.equal(m.ack, BigInt(outcome.ack))
        assert.deepEqual(m.frames, framesOf(outcome.frames))
      }
    })
  }

  for (const vec of ofKind('request_head')) {
    it(`request head ${vec.name}`, () => {
      const value = vec.value as Parameters<typeof encodeRequestHead>[0]
      assert.equal(toHex(encodeRequestHead(value)), vec.bytes_hex)
      assert.deepEqual(decodeRequestHead(fromHex(vec.bytes_hex)), value)
    })
  }

  for (const vec of ofKind('response_head')) {
    it(`response head ${vec.name}`, () => {
      const value = vec.value as Parameters<typeof encodeResponseHead>[0]
      assert.equal(toHex(encodeResponseHead(value)), vec.bytes_hex)
      assert.deepEqual(decodeResponseHead(fromHex(vec.bytes_hex)), value)
    })
  }

  for (const vec of ofKind('reset')) {
    it(`reset ${vec.name}`, () => {
      const value = vec.value as Parameters<typeof encodeReset>[0]
      assert.equal(toHex(encodeReset(value)), vec.bytes_hex)
      assert.deepEqual(decodeReset(fromHex(vec.bytes_hex)), value)
    })
  }

  for (const vec of ofKind('goaway')) {
    it(`goaway ${vec.name}`, () => {
      const value = vec.value as Parameters<typeof encodeGoaway>[0]
      assert.equal(toHex(encodeGoaway(value)), vec.bytes_hex)
      assert.deepEqual(decodeGoaway(fromHex(vec.bytes_hex)), value)
    })
  }

  for (const vec of ofKind('window')) {
    it(`window ${vec.name}`, () => {
      assert.equal(toHex(windowPayload(vec.increment as number)), vec.bytes_hex)
      assert.equal(parseWindow(fromHex(vec.bytes_hex)), vec.increment)
    })
  }

  for (const vec of ofKind('ws_message')) {
    it(`ws message ${vec.name}`, () => {
      const data = fromHex(vec.data_hex as string)
      assert.equal(toHex(wsMessage(vec.flags as number, data)), vec.bytes_hex)
      const parsed = parseWsMessage(fromHex(vec.bytes_hex))
      assert.equal(parsed.flags, vec.flags)
      assert.equal(toHex(parsed.data), vec.data_hex)
    })
  }

  for (const vec of ofKind('ws_close')) {
    it(`ws close ${vec.name}: the reason is cut on a character boundary`, () => {
      assert.equal(toHex(wsClose(vec.code as number, vec.reason as string)), vec.bytes_hex)
      const parsed = parseWsClose(fromHex(vec.bytes_hex))
      assert.equal(parsed.code, vec.code)
      assert.equal(parsed.reason, vec.kept_reason)
      assert.ok(new TextEncoder().encode(parsed.reason).length <= TUNNEL_WS_CLOSE_REASON_MAX)
    })
  }

  for (const vec of ofKind('handshake_init')) {
    it(`handshake init ${vec.name}`, () => {
      const id = fromHex(vec.ticket_id_hex as string)
      const noise = fromHex(vec.noise_hex as string)
      assert.equal(toHex(handshakeInit(id, noise)), vec.bytes_hex)
      const parsed = parseHandshakeInit(fromHex(vec.bytes_hex))
      assert.equal(toHex(parsed.ticketId), vec.ticket_id_hex)
      assert.equal(toHex(parsed.noise), vec.noise_hex)
    })
  }

  for (const vec of ofKind('handshake_reply')) {
    it(`handshake reply ${vec.name}`, () => {
      const parsed = parseHandshakeReply(fromHex(vec.bytes_hex))
      if (vec.accept) {
        assert.equal(toHex(handshakeAccept(fromHex(vec.noise_hex as string))), vec.bytes_hex)
        assert.ok(parsed.accept)
        assert.equal(toHex((parsed as { noise: Uint8Array }).noise), vec.noise_hex)
      } else {
        assert.equal(toHex(handshakeReject(vec.reason as string)), vec.bytes_hex)
        assert.deepEqual(parsed, { accept: false, reason: vec.reason })
      }
    })
  }

  for (const vec of ofKind('prologue')) {
    it(`prologue ${vec.name}`, () => {
      const bytes = previewPrologue(vec.label as string, fromHex(vec.ticket_id_hex as string))
      assert.equal(toHex(bytes), vec.bytes_hex)
    })
  }

  for (const vec of ofKind('label')) {
    it(`label ${vec.name}`, () => {
      const label = labelFromBytes(fromHex(vec.bytes_hex))
      assert.equal(label, vec.label)
      assert.ok(isValidPreviewLabel(label))
    })
  }
})

describe('tunnel codec edge cases', () => {
  it('refuses messages above the plaintext limit, both ways', () => {
    const big = newTunnelFrame(TunnelFrameType.Data, 1, new Uint8Array(TUNNEL_MAX_MESSAGE))
    assert.throws(
      () => encodeMessage(0, [big]),
      (e: unknown) => e instanceof TunnelError && e.code === 'too_large',
    )
    assert.throws(
      () => decodeMessage(new Uint8Array(TUNNEL_MAX_MESSAGE + 1)),
      (e: unknown) => e instanceof TunnelError && e.code === 'too_large',
    )
  })

  it('takes number acks and rejects out-of-range ones', () => {
    assert.equal(toHex(encodeMessage(5, [])), '0000000000000005')
    assert.throws(() => encodeMessage(-1, []), TunnelError)
    assert.throws(() => encodeMessage(1n << 64n, []), TunnelError)
  })

  it('does not alias the input buffer in decoded payloads', () => {
    const bytes = encodeMessage(0, [newTunnelFrame(TunnelFrameType.Data, 3, Uint8Array.of(1, 2))])
    const m = decodeMessage(bytes)
    bytes.fill(0)
    assert.deepEqual([...m.frames[0]!.payload], [1, 2])
  })

  it('decodes a message sitting at an offset inside a larger buffer', () => {
    const inner = encodeMessage(9, [newTunnelFrame(TunnelFrameType.End, 2)])
    const outer = new Uint8Array(inner.length + 5)
    outer.set(inner, 3)
    const m = decodeMessage(outer.subarray(3, 3 + inner.length))
    assert.equal(m.ack, 9n)
    assert.equal(m.frames[0]!.stream, 2)
  })

  it('rejects malformed heads', () => {
    assert.throws(() => decodeRequestHead(Uint8Array.of(0xc1)), TunnelError)
    assert.throws(() => decodeRequestHead(Uint8Array.of(0x91, 0x01)), TunnelError)
    assert.throws(() => decodeReset(encodeGoaway({ code: 'closed' })), TunnelError)
    assert.throws(() => decodeGoaway(encodeReset({ code: 'refused' })), TunnelError)
    assert.throws(() => decodeResponseHead(Uint8Array.of(0x80)), TunnelError)
  })

  it('omits an empty message key, like the Rust encoder', () => {
    assert.equal(
      toHex(encodeReset({ code: 'refused', message: null })),
      toHex(encodeReset({ code: 'refused' })),
    )
    assert.deepEqual(decodeReset(encodeReset({ code: 'internal', message: 'x' })), {
      code: 'internal',
      message: 'x',
    })
  })

  it('refuses oversized heads', () => {
    const headers: [string, string][] = [['x', 'a'.repeat(TUNNEL_MAX_HEAD)]]
    assert.throws(
      () => encodeRequestHead({ method: 'GET', path: '/', headers }),
      (e: unknown) => e instanceof TunnelError && e.code === 'too_large',
    )
  })

  it('handshake bodies: wrong version, short ticket id, unknown reply kind', () => {
    const id = new Uint8Array(TUNNEL_TICKET_ID_LEN).fill(9)
    const init = handshakeInit(id, Uint8Array.of(1, 2, 3))
    const wrong = init.slice()
    wrong[0] = 2
    assert.throws(
      () => parseHandshakeInit(wrong),
      (e: unknown) => e instanceof TunnelError && e.code === 'version',
    )
    assert.throws(
      () => parseHandshakeInit(init.subarray(0, 10)),
      (e: unknown) => e instanceof TunnelError && e.code === 'truncated',
    )
    assert.throws(() => handshakeInit(new Uint8Array(15), new Uint8Array(0)), TunnelError)
    assert.throws(() => parseHandshakeReply(Uint8Array.of(7)), TunnelError)
    assert.throws(() => parseHandshakeReply(new Uint8Array(0)), TunnelError)
    // 不认识的拒绝原因原样交给调用方（新版本 daemon）
    assert.deepEqual(parseHandshakeReply(handshakeReject('brand_new_reason')), {
      accept: false,
      reason: 'brand_new_reason',
    })
  })

  it('ws close: short reasons are kept, 4-byte characters are not split', () => {
    assert.equal(parseWsClose(wsClose(1000, 'bye')).reason, 'bye')
    // 124 个 ASCII 字节 + 之后的 4 字节字符：截在 123 字节
    assert.equal(parseWsClose(wsClose(1000, 'a'.repeat(200))).reason.length, 123)
    const emoji = '😀'.repeat(40) // 160 字节，30 个完整字符 = 120 字节
    assert.equal(parseWsClose(wsClose(1001, emoji)).reason, '😀'.repeat(30))
    assert.throws(() => parseWsClose(Uint8Array.of(1)), TunnelError)
    assert.throws(() => parseWsClose(Uint8Array.of(0x03, 0xe8, 0xff)), TunnelError)
    assert.throws(() => parseWsMessage(new Uint8Array(0)), TunnelError)
    assert.throws(() => parseWindow(Uint8Array.of(1, 2)), TunnelError)
    assert.throws(() => windowPayload(-1), TunnelError)
  })

  it('labelFromBytes only produces valid labels', () => {
    for (let seed = 0; seed < 50; seed++) {
      const bytes = new Uint8Array(16).map((_, i) => (seed * 37 + i * 91) & 0xff)
      const label = labelFromBytes(bytes)
      assert.ok(isValidPreviewLabel(label), label)
      assert.ok([...label].every((c) => PREVIEW_LABEL_ALPHABET.includes(c)))
    }
    assert.throws(() => labelFromBytes(new Uint8Array(15)), TunnelError)
  })
})

/** 只认 `a * b + c` 这样的整数算式（十进制或 0x 十六进制），其他一律报错，免得把不认识的表达式当成 NaN。 */
function evaluate(expr: string): number {
  return expr.split('+').reduce((sum, term) => {
    const product = term.split('*').reduce((acc, factor) => {
      const f = factor.trim().replace(/_/g, '')
      assert.match(f, /^(0x[0-9a-fA-F]+|[0-9]+)$/, `cannot evaluate ${expr}`)
      return acc * Number(f)
    }, 1)
    return sum + product
  }, 0)
}

describe('tunnel constants match crates/protocol/src/preview.rs', () => {
  const source = readFileSync(`${repoRoot}crates/protocol/src/preview.rs`, 'utf8')
  const tunnel = source.slice(source.indexOf('pub mod tunnel'))

  function rustConst(name: string): number {
    const m = new RegExp(`pub const ${name}: [a-z0-9]+ = ([^;]+);`).exec(tunnel)
    assert.ok(m, `no pub const ${name} in preview.rs`)
    return evaluate(m[1]!)
  }

  it('numeric constants', () => {
    const pairs: Array<[string, number]> = [
      ['TICKET_ID_LEN', TUNNEL_TICKET_ID_LEN],
      ['PSK_LEN', TUNNEL_PSK_LEN],
      ['HANDSHAKE_VERSION', TUNNEL_HANDSHAKE_VERSION],
      ['HANDSHAKE_ACCEPT', TUNNEL_HANDSHAKE_ACCEPT],
      ['HANDSHAKE_REJECT', TUNNEL_HANDSHAKE_REJECT],
      ['MAX_MESSAGE', TUNNEL_MAX_MESSAGE],
      ['MAX_DATA_FRAME', TUNNEL_MAX_DATA_FRAME],
      ['MAX_HEAD', TUNNEL_MAX_HEAD],
      ['MAX_WS_MESSAGE', TUNNEL_MAX_WS_MESSAGE],
      ['STREAM_WINDOW', TUNNEL_STREAM_WINDOW],
      ['MAX_STREAMS', TUNNEL_MAX_STREAMS],
      ['DAEMON_INFLIGHT', TUNNEL_DAEMON_INFLIGHT],
      ['CLIENT_INFLIGHT', TUNNEL_CLIENT_INFLIGHT],
      ['ACK_EVERY_BYTES', TUNNEL_ACK_EVERY_BYTES],
      ['ACK_EVERY_MESSAGES', TUNNEL_ACK_EVERY_MESSAGES],
      ['ACK_DELAY_MS', TUNNEL_ACK_DELAY_MS],
      ['WS_CLOSE_REASON_MAX', TUNNEL_WS_CLOSE_REASON_MAX],
      ['WS_BINARY', TUNNEL_WS_BINARY],
      ['WS_FIN', TUNNEL_WS_FIN],
      ['FRAME_HEADER', TUNNEL_FRAME_HEADER],
    ]
    for (const [name, value] of pairs) assert.equal(value, rustConst(name), `TUNNEL_${name}`)
  })

  it('prologue prefix and label alphabet', () => {
    assert.ok(tunnel.includes(`PROLOGUE_PREFIX: &[u8] = b"${TUNNEL_PROLOGUE_PREFIX}";`))
    assert.ok(tunnel.includes(`LABEL_ALPHABET: &[u8] = b"${PREVIEW_LABEL_ALPHABET}";`))
  })

  it('frame type bytes', () => {
    const rust = new Map<string, number>()
    for (const m of tunnel.matchAll(/^\s+(\w+) = (0x[0-9a-f]+),$/gm)) rust.set(m[1]!, Number(m[2]))
    assert.equal(rust.size, Object.keys(TunnelFrameType).length)
    for (const [name, value] of Object.entries(TunnelFrameType)) {
      assert.equal(rust.get(name), value, `TunnelFrameType.${name}`)
    }
  })
})
