// 「稍后」段里的待确认规则的推导（docs/spec/daemon.md § 23，spec/web.md § 4.11）：只有 `pending`、只算勾选的节点、
// 和 agent 记的项合在一起按等待时间排、不进「现在」的数字、不认识的状态不在这里。
import { describe, expect, it } from 'vitest'
import { ruleItem } from '@/test/fakeRulesDaemon'
import type { ViewRule } from '@/features/rules/store'
import { todoItem } from '@/test/factories'
import type { ViewTodo } from '@/features/todos/types'
import { deriveInbox, inboxNowCount, type InboxInputs } from './derive'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const rule = (id: string, over: Partial<ViewRule> = {}): ViewRule => ({ ...ruleItem({ id, status: 'pending', created_ms: 100 }), node_id: A, ...over })
const todo = (id: string, created: number): ViewTodo => ({ ...todoItem({ id, source: { kind: 'agent' }, unreviewed: true, created_ms: created }), node_id: A, unsynced: false }) as ViewTodo
const derive = (over: Partial<InboxInputs>) => deriveInbox({ sessions: {}, pending: {}, enroll: { items: {}, resolved: {} }, selected: [A], now: 1_000_000, ...over })
const later = (m: ReturnType<typeof derive>) => m.sections.find((s) => s.id === 'later')!.items

describe('rules in Later', () => {
  it('lists pending rules only, one item per rule, in the later section', () => {
    const m = derive({ rules: [rule('p1'), rule('a', { status: 'active' }), rule('d', { status: 'disabled' }), rule('r', { status: 'rejected' }), rule('u', { status: 'expired' as never })] })
    expect(later(m).map((i) => [i.key, i.kind])).toEqual([[`rule:${A}:p1`, 'rule']])
    expect(later(m)[0]!.section).toBe('later')
  })

  it('only the selected nodes', () => {
    const m = derive({ rules: [rule('p1'), rule('p2', { node_id: B })], selected: [B] })
    expect(later(m).map((i) => i.key)).toEqual([`rule:${B}:p2`])
  })

  it('is ordered with the agent’s to-dos by how long it has waited', () => {
    const m = derive({ rules: [rule('old', { created_ms: 50 }), rule('new', { created_ms: 300 })], todos: [todo('mid', 200)] })
    expect(later(m).map((i) => i.key)).toEqual([`rule:${A}:old`, `later:${A}:mid`, `rule:${A}:new`])
  })

  it('never changes the number that counts toward the badge', () => {
    expect(inboxNowCount(derive({ rules: [rule('p1'), rule('p2')] }))).toBe(0)
  })
})
