// 空间的推导和选择语义（spec/web.md § 4.13）：分组去掉首尾空白、区分大小写；显示用清理过的文字、身份用原文；
// 选一个空间是「这个空间」这个概念，它的节点集合跟着分组走；手点的一组节点就是手点的一组；没有分组时和旧的节点过滤器一模一样。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { deriveSpaces, groupError, groupOf, resolveSelection, spaceLabelOf } from './spaces'

const info = (group?: string | null) => ({ info: { group } as never })
const A = 'a'
const B = 'b'
const C = 'c'
const D = 'd'

describe('deriveSpaces', () => {
  it('groups nodes by the trimmed, case-sensitive group name; nodes without a group form no named space', () => {
    const m = deriveSpaces([A, B, C, D], { a: info(' Home '), b: info('Home'), c: info('home'), d: info('') })
    expect(m.spaces.map((s) => [s.key, s.nodeIds])).toEqual([
      ['Home', ['a', 'b']],
      ['home', ['c']],
    ])
    expect(m.ungrouped).toEqual(['d'])
    expect(m.hasGroups).toBe(true)
  })

  it('without any group there is no space at all (the plain node filter)', () => {
    expect(deriveSpaces([A, B], { a: info(), b: info(null) })).toEqual({ spaces: [], ungrouped: ['a', 'b'], hasGroups: false })
    expect(deriveSpaces([A], { a: info('   ') }).hasGroups).toBe(false)
  })

  it('a node whose node.info is not known yet counts as having no group', () => {
    const m = deriveSpaces([A, B], { a: info('Work'), b: undefined })
    expect(m.spaces.map((s) => s.nodeIds)).toEqual([['a']])
    expect(m.ungrouped).toEqual(['b'])
    expect(deriveSpaces([A], { a: {} }).hasGroups).toBe(false)
  })

  it('orders spaces by their first node in pairing order', () => {
    expect(deriveSpaces([A, B, C], { a: info('Z'), b: info('A'), c: info('Z') }).spaces.map((s) => s.key)).toEqual(['Z', 'A'])
  })

  it('the identity is the raw trimmed text, the label is cleaned: control and bidi characters never reach the display, and two names that clean to the same text stay two spaces with distinct labels', () => {
    const m = deriveSpaces([A, B, C], { a: info('Ho\u202Eme'), b: info('Ho\u0007me'), c: info('Home') })
    expect(m.spaces.map((s) => s.key)).toEqual(['Ho\u202Eme', 'Ho\u0007me', 'Home'])
    const labels = m.spaces.map((s) => s.label)
    expect(new Set(labels).size).toBe(3)
    for (const l of labels) {
      expect(l).not.toContain('\u202E')
      expect(l).not.toContain('\u0007')
    }
    expect(labels[0]).toBe('Ho me')
  })

  it('a very long name is truncated for display only', () => {
    const long = 'x'.repeat(200)
    const m = deriveSpaces([A], { a: info(long) })
    expect(m.spaces[0]!.key).toHaveLength(200)
    expect([...m.spaces[0]!.label]).toHaveLength(40)
  })

  it('spaceLabelOf finds the space of a node, or empty', () => {
    const m = deriveSpaces([A, B], { a: info('Home'), b: info() })
    expect(spaceLabelOf(m, A)).toBe('Home')
    expect(spaceLabelOf(m, B)).toBe('')
  })
})

describe('resolveSelection', () => {
  const order = [A, B, C]
  const grouped = { a: info('Home'), b: info('Work'), c: info('Work') }

  it('without a space it is the old node filter: hand-picked nodes, or all; a pick with no existing node falls back to all', () => {
    expect(resolveSelection(order, {}, null, null)).toEqual(['a', 'b', 'c'])
    expect(resolveSelection(order, {}, ['b'], null)).toEqual(['b'])
    expect(resolveSelection(order, {}, ['gone'], null)).toEqual(['a', 'b', 'c'])
    // 之后配对的新节点：「全部」包含它，手点的不包含
    expect(resolveSelection([A, B, C, D], {}, null, null)).toEqual(['a', 'b', 'c', 'd'])
    expect(resolveSelection([A, B, C, D], {}, ['a'], null)).toEqual(['a'])
  })

  it('a space selects exactly its nodes, and its node set follows the groups: a node that joins it is in, a node that leaves is out', () => {
    expect(resolveSelection(order, grouped, null, 'Work')).toEqual(['b', 'c'])
    expect(resolveSelection(order, { ...grouped, a: info('Work') }, null, 'Work')).toEqual(['a', 'b', 'c'])
    expect(resolveSelection(order, { ...grouped, c: info('Home') }, null, 'Work')).toEqual(['b'])
    // 新配对的节点（带着这个分组）也在
    expect(resolveSelection([A, B, C, D], { ...grouped, d: info('Work') }, null, 'Work')).toEqual(['b', 'c', 'd'])
    // 节点被忘记
    expect(resolveSelection([A, B], grouped, null, 'Work')).toEqual(['b'])
  })

  it('a hand-picked set stays hand-picked when groups change', () => {
    expect(resolveSelection(order, grouped, ['a', 'b'], null)).toEqual(['a', 'b'])
    expect(resolveSelection(order, { ...grouped, a: info('Work') }, ['a', 'b'], null)).toEqual(['a', 'b'])
  })

  it('when the selected space has no node any more (renamed away, group cleared) it falls back to the hand-picked set or all; the stored name keeps working if it comes back', () => {
    expect(resolveSelection(order, { a: info('Home'), b: info('Home'), c: info('Home') }, null, 'Work')).toEqual(['a', 'b', 'c'])
    expect(resolveSelection(order, { a: info('Home'), b: info('Home'), c: info('Home') }, ['b'], 'Work')).toEqual(['b'])
    expect(resolveSelection(order, grouped, null, 'Work')).toEqual(['b', 'c'])
  })

  it('with no group on any node the stored space is ignored: exactly the node filter', () => {
    expect(resolveSelection(order, {}, null, 'Work')).toEqual(['a', 'b', 'c'])
    expect(resolveSelection(order, { a: info(), b: info(), c: info() }, ['c'], 'Work')).toEqual(['c'])
  })

  it('space identity is case-sensitive and trimmed', () => {
    expect(resolveSelection(order, { a: info(' Work '), b: info('work'), c: info() }, null, 'Work')).toEqual(['a'])
  })
})

describe('groupOf and groupError', () => {
  it('groupOf trims; absent is empty', () => {
    expect(groupOf({ group: '  Home ' })).toBe('Home')
    expect(groupOf({})).toBe('')
    expect(groupOf(null)).toBe('')
  })

  it('validates like the daemon: empty is fine (no group), at most 40 code points, one line, no control characters', () => {
    expect(groupError('')).toBeNull()
    expect(groupError('   ')).toBeNull()
    expect(groupError('Home')).toBeNull()
    expect(groupError('x'.repeat(40))).toBeNull()
    expect(groupError('x'.repeat(41))).toBe('too_long')
    // 40 个码点，但 UTF-16 长度 80：按码点算
    expect(groupError('\u{1F600}'.repeat(40))).toBeNull()
    expect(groupError('\u{1F600}'.repeat(41))).toBe('too_long')
    expect(groupError('a\nb')).toBe('control')
    expect(groupError('a\tb')).toBe('control')
    expect(groupError(`a${String.fromCharCode(0x2028)}b`)).toBe('control')
    expect(groupError(`a${String.fromCharCode(0x2029)}b`)).toBe('control')
  })
})

describe('the stored preference', () => {
  beforeEach(() => vi.resetModules())
  afterEach(() => localStorage.clear())

  it('an old stored picked-nodes preference keeps its meaning: no migration needed, no space is selected', async () => {
    localStorage.setItem('anycon.prefs', JSON.stringify({ state: { sidebarNodes: ['a', 'b'], theme: 'dark' }, version: 1 }))
    const { usePrefs } = await import('@/stores/prefs')
    expect(usePrefs.getState().sidebarNodes).toEqual(['a', 'b'])
    expect(usePrefs.getState().sidebarSpace).toBeNull()
    expect(usePrefs.getState().lastTodoNodeBySpace).toEqual({})
  })

  it('an old stored “all nodes” preference stays all', async () => {
    localStorage.setItem('anycon.prefs', JSON.stringify({ state: { sidebarNodes: null }, version: 1 }))
    const { usePrefs } = await import('@/stores/prefs')
    expect(usePrefs.getState().sidebarNodes).toBeNull()
    expect(usePrefs.getState().sidebarSpace).toBeNull()
  })

  it('the space choice is stored on this device and survives a reload', async () => {
    const first = await import('@/stores/prefs')
    first.usePrefs.getState().set({ sidebarSpace: 'Work' })
    vi.resetModules()
    const second = await import('@/stores/prefs')
    expect(second.usePrefs.getState().sidebarSpace).toBe('Work')
  })
})
