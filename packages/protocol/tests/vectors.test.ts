// Rust 侧（tools/gen-vectors）生成的字节级 fixture：TS 解码、再编码必须逐字节一致。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import {
  CONTROL_TYPES,
  decodeControl,
  decodeEnvelope,
  decodeRpc,
  encodeControl,
  encodeEnvelope,
  encodeRpc,
} from '../src/index.js'
import { fromHex, loadVectors, sortKeysDeep, toHex } from './helpers.js'

const vectors = loadVectors()

describe('Rust 生成的 msgpack / JSON-RPC 向量', () => {
  it('manifest 非空，且每个 Control 变体都有向量', () => {
    assert.ok(vectors.length >= 30)
    const covered = new Set(
      vectors.filter((v) => v.type === 'control').map((v) => (v.value as { type: string }).type),
    )
    for (const t of CONTROL_TYPES) assert.ok(covered.has(t), `缺少 control 向量: ${t}`)
  })

  for (const vec of vectors.filter((v) => v.type === 'envelope')) {
    it(`envelope ${vec.name}: 解码值正确，重编码逐字节一致`, () => {
      const env = decodeEnvelope(vec.bytes)
      const expected = vec.value as {
        v: number
        from: string
        to: string
        kind: string
        seq: number
        body_hex: string
      }
      assert.equal(env.v, expected.v)
      assert.equal(env.from, expected.from)
      assert.equal(env.to, expected.to)
      assert.equal(env.kind, expected.kind)
      assert.equal(env.seq, expected.seq)
      assert.equal(toHex(env.body), expected.body_hex)
      assert.equal(toHex(encodeEnvelope(env)), toHex(vec.bytes))
    })
  }

  for (const vec of vectors.filter((v) => v.type === 'control')) {
    it(`control ${vec.name}: 解码值正确，重编码逐字节一致`, () => {
      const ctrl = decodeControl(vec.bytes)
      assert.deepEqual(ctrl, vec.value)
      assert.equal(toHex(encodeControl(ctrl)), toHex(vec.bytes))
    })

    it(`control ${vec.name}: 字母序对象（generated.ts 的写法）编码后键序与 Rust 一致`, () => {
      const alphabetical = sortKeysDeep(vec.value) as unknown as Parameters<typeof encodeControl>[0]
      assert.equal(toHex(encodeControl(alphabetical)), toHex(vec.bytes))
    })

    it(`control ${vec.name}: 可选字段为 null / undefined 时与省略等价`, () => {
      const ctrl = decodeControl(vec.bytes) as Record<string, unknown>
      // 把 Rust 里 skip_serializing_if 的缺省字段显式写成 null / undefined，编码结果不变。
      const withNulls: Record<string, unknown> = { ...ctrl }
      if (ctrl.type === 'auth') {
        withNulls.device_token ??= null
        const client = { ...(ctrl.client as Record<string, unknown>) }
        client.origin ??= undefined
        withNulls.client = client
      }
      if (ctrl.type === 'auth_ok') withNulls.quota ??= null
      if (ctrl.type === 'pair_done') {
        withNulls.device_id ??= null
        withNulls.device_token ??= undefined
      }
      assert.equal(
        toHex(encodeControl(withNulls as unknown as Parameters<typeof encodeControl>[0])),
        toHex(vec.bytes),
      )
    })
  }

  for (const vec of vectors.filter((v) => v.type === 'rpc')) {
    it(`rpc ${vec.name}: 解码值正确，重编码逐字节一致`, () => {
      const msg = decodeRpc(vec.bytes)
      assert.deepEqual(msg, vec.value)
      assert.equal(toHex(encodeRpc(msg)), toHex(vec.bytes))
    })

    it(`rpc ${vec.name}: 乱序键的消息重编码后键序与 Rust 一致`, () => {
      const shuffled = sortKeysDeep(vec.value) as unknown as Parameters<typeof encodeRpc>[0]
      assert.equal(toHex(encodeRpc(shuffled)), toHex(vec.bytes))
    })
  }

  it('hex 文件是小写、单行', () => {
    for (const vec of vectors) {
      assert.match(toHex(vec.bytes), /^[0-9a-f]+$/)
      assert.deepEqual(fromHex(toHex(vec.bytes)), vec.bytes)
    }
  })
})
