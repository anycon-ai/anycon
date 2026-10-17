// data 信封 body 框架（ADR-30）：和 Rust `envelope::frame` 的字节级一致性（tests/vectors/frames.json，
// 由 tools/gen-vectors 用 Rust 生成）+ 重组器的行为。
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { describe, it } from 'node:test'

import {
  FRAME_FRAGMENT,
  FRAME_RESET,
  FRAME_WHOLE,
  FrameError,
  MAX_FRAGMENT,
  MAX_MESSAGE,
  ProtocolError,
  Reassembler,
  decodeFrame,
  encodeFrames,
  pushBody,
  resetBody,
  type DataFrame,
} from '../src/index.js'
import { fromHex, toHex } from './helpers.js'

/** 与 gen-vectors 里 `pattern()` 相同的确定性密文：`(i % 251) as u8`。 */
function pattern(len: number): Uint8Array {
  return Uint8Array.from({ length: len }, (_, i) => i % 251)
}

type Outcome =
  | { ok: 'whole'; data_hex: string }
  | { ok: 'reset' }
  | { ok: 'fragment'; msg_id: number; index: number; count: number; data_hex: string }
  | { err: string }

type FrameVector =
  | {
      kind: 'encode'
      name: string
      msg_id: number
      ciphertext_len: number
      frames: Array<{ header_hex: string; data_len: number; body_hex: string | null }>
    }
  | { kind: 'reset_body'; name: string; body_hex: string }
  | { kind: 'decode'; name: string; body_hex: string; outcome: Outcome }
  | {
      kind: 'reassemble'
      name: string
      steps: Array<{
        body_hex: string
        out_hex?: string
        out?: null
        err?: string
        decode_err?: string
      }>
    }

const vectors = JSON.parse(
  readFileSync(new URL('./vectors/frames.json', import.meta.url), 'utf8'),
) as FrameVector[]

function outcomeOf(body: Uint8Array): Outcome {
  try {
    const f = decodeFrame(body)
    if (f.kind === 'whole') return { ok: 'whole', data_hex: toHex(f.data) }
    if (f.kind === 'reset') return { ok: 'reset' }
    return {
      ok: 'fragment',
      msg_id: f.msgId,
      index: f.index,
      count: f.count,
      data_hex: toHex(f.data),
    }
  } catch (err) {
    if (err instanceof FrameError) return { err: err.code }
    throw err
  }
}

describe('Rust 生成的 frame 向量', () => {
  it('向量非空，覆盖 encode / decode / reassemble / reset', () => {
    assert.ok(vectors.length >= 30)
    for (const kind of ['encode', 'decode', 'reassemble', 'reset_body'] as const) {
      assert.ok(
        vectors.some((v) => v.kind === kind),
        kind,
      )
    }
  })

  for (const vec of vectors) {
    if (vec.kind === 'encode') {
      it(`encode ${vec.name}: 每个 body 与 Rust 逐字节一致`, () => {
        const ct = pattern(vec.ciphertext_len)
        const bodies = encodeFrames(ct, vec.msg_id)
        assert.equal(bodies.length, vec.frames.length)
        let offset = 0
        bodies.forEach((body, i) => {
          const want = vec.frames[i]!
          const headerLen = fromHex(want.header_hex).length
          assert.equal(toHex(body.subarray(0, headerLen)), want.header_hex, `body ${i} header`)
          assert.equal(body.length - headerLen, want.data_len, `body ${i} data length`)
          assert.deepEqual(body.subarray(headerLen), ct.subarray(offset, offset + want.data_len))
          if (want.body_hex !== null) assert.equal(toHex(body), want.body_hex)
          offset += want.data_len
        })
        assert.equal(offset, ct.length)
        // 每片都不超过 MAX_FRAGMENT 的载荷
        for (const body of bodies) assert.ok(body.length <= MAX_FRAGMENT + 9)
      })

      it(`encode ${vec.name}: 重组后得到原密文`, () => {
        const ct = pattern(vec.ciphertext_len)
        const r = new Reassembler()
        let out: Uint8Array | undefined
        for (const body of encodeFrames(ct, vec.msg_id)) out = pushBody(r, body)
        assert.deepEqual(out, ct)
      })
    } else if (vec.kind === 'reset_body') {
      it('reset_body 与 Rust 一致', () => {
        assert.equal(toHex(resetBody()), vec.body_hex)
      })
    } else if (vec.kind === 'decode') {
      it(`decode ${vec.name}: 结果与 Rust 一致`, () => {
        assert.deepEqual(outcomeOf(fromHex(vec.body_hex)), vec.outcome)
      })
    } else {
      it(`reassemble ${vec.name}: 每一步（含出错后的状态）与 Rust 一致`, () => {
        const r = new Reassembler()
        for (const [i, step] of vec.steps.entries()) {
          const label = `step ${i}`
          let frame: DataFrame
          try {
            frame = decodeFrame(fromHex(step.body_hex))
          } catch (err) {
            assert.ok(err instanceof FrameError, label)
            assert.equal(err.code, step.decode_err, label)
            continue
          }
          try {
            const got = r.push(frame)
            assert.equal(step.err, undefined, `${label}: Rust 报错 ${step.err}，TS 没有`)
            if (step.out_hex !== undefined) assert.equal(toHex(got!), step.out_hex, label)
            else assert.equal(got, undefined, label)
          } catch (err) {
            assert.ok(err instanceof FrameError, label)
            assert.equal(err.code, step.err, label)
          }
        }
      })
    }
  }
})

/** 跑 `fn`，返回它抛出的 `FrameError` 的 code（没抛返回 undefined）。 */
function code(fn: () => unknown): string | undefined {
  try {
    fn()
  } catch (err) {
    assert.ok(err instanceof FrameError, String(err))
    return err.code
  }
  return undefined
}

describe('frame 行为', () => {
  it('常量', () => {
    assert.equal(FRAME_WHOLE, 0)
    assert.equal(FRAME_FRAGMENT, 1)
    assert.equal(FRAME_RESET, 2)
    assert.equal(MAX_FRAGMENT, 200 * 1024)
    assert.equal(MAX_MESSAGE, 16 * 1024 * 1024)
  })

  it('FrameError 是 ProtocolError', () => {
    assert.throws(() => decodeFrame(new Uint8Array()), ProtocolError)
  })

  it('1 MiB 往返：6 片，前 5 片满、最后一片是余量', () => {
    const ct = pattern(1024 * 1024)
    const bodies = encodeFrames(ct, 77)
    assert.equal(bodies.length, 6)
    const r = new Reassembler()
    const results = bodies.map((b) => pushBody(r, b))
    assert.deepEqual(
      results.map((x) => x === undefined),
      [true, true, true, true, true, false],
    )
    assert.deepEqual(results[5], ct)
  })

  it('msg_id 必须是 u32', () => {
    assert.throws(() => encodeFrames(new Uint8Array(1), -1), ProtocolError)
    assert.throws(() => encodeFrames(new Uint8Array(1), 2 ** 32), ProtocolError)
    assert.throws(() => encodeFrames(new Uint8Array(1), 1.5), ProtocolError)
    assert.equal(encodeFrames(new Uint8Array(1), 2 ** 32 - 1).length, 1)
  })

  it('decodeFrame 的 data 是 body 的视图；Reassembler 返回的是拷贝', () => {
    const body = Uint8Array.of(FRAME_WHOLE, 1, 2, 3)
    const f = decodeFrame(body)
    assert.equal(f.kind, 'whole')
    const out = new Reassembler().push(f)!
    body[1] = 99
    assert.deepEqual(out, Uint8Array.of(1, 2, 3))
  })

  it('超过 MAX_MESSAGE 报 too_large，并丢弃进行中的重组', () => {
    const r = new Reassembler()
    const chunk = new Uint8Array(8 * 1024 * 1024)
    const mk = (index: number): DataFrame => ({
      kind: 'fragment',
      msgId: 1,
      index,
      count: 10,
      data: chunk,
    })
    assert.equal(r.push(mk(0)), undefined)
    assert.equal(r.push(mk(1)), undefined) // 恰好 16 MiB，不超
    assert.throws(
      () => r.push(mk(2)),
      (err: unknown) => err instanceof FrameError && err.code === 'too_large',
    )
    // 状态已清：必须从 index 0 重新开始
    assert.throws(
      () => r.push(mk(3)),
      (err: unknown) => err instanceof FrameError && err.code === 'out_of_order',
    )
    assert.equal(r.push(mk(0)), undefined)
  })

  // 对照 crates/protocol/src/envelope.rs 的 `reassembler_rejects_hostile_headers_without_panicking`。
  it('恶意头部——index >= count、count 中途变化、u16 边界、count = 1、完成后同 msg_id 再来', () => {
    const x = Uint8Array.of(0x78)
    const frag = (msgId: number, index: number, count: number, data = x): DataFrame => ({
      kind: 'fragment',
      msgId,
      index,
      count,
      data,
    })
    // index >= count（包括 index = 65535）
    let r = new Reassembler()
    assert.equal(
      code(() => r.push(frag(1, 2, 2))),
      'out_of_order',
    )
    assert.equal(
      code(() => r.push(frag(1, 65535, 2))),
      'out_of_order',
    )

    // count 中途变化（变大 / 变小）：CountMismatch，之后状态已清、必须从 0 开始
    r = new Reassembler()
    assert.equal(r.push(frag(7, 0, 3)), undefined)
    assert.equal(
      code(() => r.push(frag(7, 1, 4))),
      'count_mismatch',
    )
    assert.equal(
      code(() => r.push(frag(7, 1, 3))),
      'out_of_order',
    )
    r = new Reassembler()
    assert.equal(r.push(frag(7, 0, 3)), undefined)
    assert.equal(
      code(() => r.push(frag(7, 1, 2))),
      'count_mismatch',
    )

    // 中途来一个 index >= count：报错并清掉进行中的重组
    r = new Reassembler()
    assert.equal(r.push(frag(1, 0, 3)), undefined)
    assert.equal(
      code(() => r.push(frag(1, 3, 3))),
      'out_of_order',
    )
    assert.equal(
      code(() => r.push(frag(1, 1, 3))),
      'out_of_order',
    )

    // count = 0：头部非法
    r = new Reassembler()
    assert.equal(
      code(() => r.push(frag(1, 0, 0))),
      'count_mismatch',
    )

    // count = u16::MAX：走到最后一片也不会溢出（用空片省内存）
    r = new Reassembler()
    const empty = (index: number) => frag(9, index, 0xffff, new Uint8Array(0))
    for (let i = 0; i < 0xffff - 1; i++) assert.equal(r.push(empty(i)), undefined)
    assert.deepEqual(r.push(empty(0xffff - 1)), new Uint8Array(0))

    // count = 1 的单片 FRAGMENT 也能收；同一 msg_id 可以再来一条（都从 0 开始）
    r = new Reassembler()
    assert.deepEqual(r.push(frag(3, 0, 1)), x)
    assert.deepEqual(r.push(frag(3, 0, 1)), x)
    // 一条完成后，同一 msg_id 的下一片要从 0 开始
    assert.equal(
      code(() => r.push(frag(3, 1, 2))),
      'out_of_order',
    )
  })

  it('clear() 丢弃进行中的重组', () => {
    const r = new Reassembler()
    const [a, b] = encodeFrames(pattern(MAX_FRAGMENT + 5), 3)
    assert.equal(pushBody(r, a!), undefined)
    r.clear()
    assert.throws(() => pushBody(r, b!), FrameError)
  })

  it('decodeFrame 能处理 byteOffset 不为 0 的视图（WebSocket 缓冲的一部分）', () => {
    const [first] = encodeFrames(pattern(MAX_FRAGMENT + 3), 0xdeadbeef)
    const padded = new Uint8Array(first!.length + 11)
    padded.set(first!, 7)
    const view = padded.subarray(7, 7 + first!.length)
    assert.notEqual(view.byteOffset, 0)
    const f = decodeFrame(view)
    assert.ok(f.kind === 'fragment')
    assert.equal(f.msgId, 0xdeadbeef)
    assert.equal(f.index, 0)
    assert.equal(f.count, 2)
    assert.equal(f.data.length, MAX_FRAGMENT)
  })

  it('RESET 帧 body 就是一个字节', () => {
    assert.deepEqual(resetBody(), Uint8Array.of(2))
    assert.deepEqual(decodeFrame(resetBody()), { kind: 'reset' })
  })
})
