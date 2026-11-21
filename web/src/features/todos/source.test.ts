// 一项是谁记的：认识的两种人（device / local）、agent，以及不认识的值（更新的 daemon 才有的来源）。
import { describe, expect, it } from 'vitest'
import { isAgentSource, isHumanSource, isOtherSource } from './source'

const kinds = ['device', 'local', 'agent', 'twin', '', 'unknown'] as const

describe('source kinds', () => {
  it('only device and local are people; agent is an agent; everything else is “other”, never “mine”', () => {
    const rows = kinds.map((k) => [k, isHumanSource({ kind: k as never }), isAgentSource({ kind: k as never }), isOtherSource({ kind: k as never })])
    expect(rows).toEqual([
      ['device', true, false, false],
      ['local', true, false, false],
      ['agent', false, true, false],
      ['twin', false, false, true],
      ['', false, false, true],
      ['unknown', false, false, true],
    ])
  })
})
