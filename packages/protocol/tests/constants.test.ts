// 漂移守卫：解析 crates/protocol 的 Rust 源码，断言 constants.ts / envelope.ts 与之一致。
// Rust 侧加了方法名、Control 变体、close code、错误码而这里没跟上，测试就会红。
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { describe, it } from 'node:test'

import {
  CLOSE_CODE,
  CONTROL_TYPES,
  ENVELOPE_KINDS,
  ENVELOPE_VERSION,
  ERROR_CODE_NUMBERS,
  FRAME_FRAGMENT,
  FRAME_RESET,
  FRAME_WHOLE,
  MAX_FRAGMENT,
  MAX_MESSAGE,
  METHODS,
  PAIR_CODE_ALPHABET,
  PAIR_CODE_LEN,
  PAIR_CODE_LEN_LONG,
  PROTOCOL_VERSION,
} from '../src/index.js'
import { repoRoot } from './helpers.js'

const rust = (file: string) => readFileSync(`${repoRoot}crates/protocol/src/${file}`, 'utf8')

const snake = (camel: string) => camel.replace(/(?<!^)([A-Z])/g, '_$1').toLowerCase()

/** 截取 `start` 那行之后、到缩进为 0 的 `}` 为止的块。 */
function block(source: string, start: RegExp): string {
  const lines = source.split('\n')
  const from = lines.findIndex((l) => start.test(l))
  assert.ok(from >= 0, `在 Rust 源码里找不到 ${start}`)
  const body: string[] = []
  for (let i = from + 1; i < lines.length; i++) {
    if (lines[i] === '}') break
    body.push(lines[i]!)
  }
  return body.join('\n')
}

describe('constants.ts 与 crates/protocol 一致', () => {
  it('methods.rs 的每个方法名常量都在 METHODS 里，反之亦然', () => {
    const src = rust('methods.rs')
    const rustMethods = new Map<string, string>()
    for (const m of src.matchAll(/pub const ([A-Z0-9_]+): &str = "([^"]+)";/g)) {
      rustMethods.set(m[1]!, m[2]!)
    }
    assert.ok(rustMethods.size > 30, `只解析到 ${rustMethods.size} 个常量，正则可能失效`)
    for (const [name, value] of rustMethods) {
      assert.equal(
        (METHODS as Record<string, string>)[name],
        value,
        `METHODS.${name} 应为 ${value}（methods.rs）`,
      )
    }
    for (const name of Object.keys(METHODS)) {
      assert.ok(rustMethods.has(name), `METHODS.${name} 在 methods.rs 里不存在`)
    }
  })

  it('方法名值唯一', () => {
    const values = Object.values(METHODS)
    assert.equal(new Set(values).size, values.length)
  })

  it('版本号与短码常量', () => {
    const lib = rust('lib.rs')
    assert.equal(
      Number(/pub const PROTOCOL_VERSION: u32 = (\d+);/.exec(lib)?.[1]),
      PROTOCOL_VERSION,
    )
    assert.equal(Number(/pub const ENVELOPE_VERSION: u8 = (\d+);/.exec(lib)?.[1]), ENVELOPE_VERSION)
    assert.equal(
      /pub const PAIR_CODE_ALPHABET: &str = "([^"]+)";/.exec(lib)?.[1],
      PAIR_CODE_ALPHABET,
    )
    assert.equal(Number(/pub const PAIR_CODE_LEN: usize = (\d+);/.exec(lib)?.[1]), PAIR_CODE_LEN)
  })

  it('长短码长度与 crates/crypto 一致', () => {
    const crypto = readFileSync(`${repoRoot}crates/crypto/src/lib.rs`, 'utf8')
    assert.equal(
      Number(/pub const PAIR_CODE_LEN_LONG: usize = (\d+);/.exec(crypto)?.[1]),
      PAIR_CODE_LEN_LONG,
    )
  })

  it('frame 常量（flags、片大小、重组上限）与 envelope.rs 的 mod frame 一致', () => {
    const mod = block(rust('envelope.rs'), /pub mod frame \{/)
    const u8 = (name: string) =>
      Number(new RegExp(`pub const ${name}: u8 = (0x[0-9a-fA-F]+|\\d+);`).exec(mod)?.[1])
    assert.equal(u8('WHOLE'), FRAME_WHOLE)
    assert.equal(u8('FRAGMENT'), FRAME_FRAGMENT)
    assert.equal(u8('RESET'), FRAME_RESET)
    const size = (name: string) => {
      const m = new RegExp(`pub const ${name}: usize = ([0-9 *]+);`).exec(mod)
      assert.ok(m, `找不到 frame::${name}`)
      return m[1]!.split('*').reduce((acc, n) => acc * Number(n.trim()), 1)
    }
    assert.equal(size('MAX_FRAGMENT'), MAX_FRAGMENT)
    assert.equal(size('MAX_MESSAGE'), MAX_MESSAGE)
  })

  it('close code', () => {
    const mod = block(rust('control.rs'), /pub mod close_code \{/)
    const rustCodes: Record<string, number> = {}
    for (const m of mod.matchAll(/pub const ([A-Z_]+): u16 = (\d+);/g))
      rustCodes[m[1]!] = Number(m[2])
    assert.deepEqual(CLOSE_CODE, rustCodes)
  })

  it('ErrorCode → 数字 code', () => {
    const src = rust('error.rs')
    const rustCodes: Record<string, number> = {}
    for (const m of src.matchAll(/ErrorCode::([A-Za-z]+) => (-\d+),/g)) {
      rustCodes[snake(m[1]!)] = Number(m[2])
    }
    assert.ok(Object.keys(rustCodes).length >= 12)
    assert.deepEqual(ERROR_CODE_NUMBERS, rustCodes)
  })

  it('Control 变体与 envelope 编码表一致', () => {
    const variants = [
      ...block(rust('control.rs'), /pub enum Control \{/).matchAll(/^ {4}([A-Z][A-Za-z0-9]*)\b/gm),
    ].map((m) => snake(m[1]!))
    assert.ok(variants.length >= 21)
    assert.deepEqual(CONTROL_TYPES.toSorted(), variants.toSorted())
  })

  it('信封 Kind 变体一致', () => {
    const variants = [
      ...block(rust('envelope.rs'), /pub enum Kind \{/).matchAll(/^ {4}([A-Z][A-Za-z]*),/gm),
    ].map((m) => snake(m[1]!))
    assert.deepEqual(ENVELOPE_KINDS.toSorted(), variants.toSorted())
  })
})
