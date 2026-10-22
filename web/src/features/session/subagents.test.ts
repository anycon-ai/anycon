// 子 agent 的调用按 parent_id 归组：只认排在父调用之后的，计数含嵌套，后台子 agent 等补的结果才算完。
import { beforeEach, describe, expect, it } from 'vitest'
import { evs, resetSeq } from '@/test/factories'
import { buildView, type ToolBlock } from './reducer'
import { countCalls, subagentRows, subagentState, toolTree } from './subagents'

beforeEach(() => resetSeq())

const call = (id: string, parent_id?: string, category: 'shell' | 'subagent' = 'shell') =>
  ({ type: 'tool_call', id, name: category === 'subagent' ? 'Agent' : 'Bash', category, title: id, input: {}, ...(parent_id ? { parent_id } : {}) }) as const

describe('toolTree', () => {
  it('groups calls under the call that started them, nested agents included', () => {
    const view = buildView(evs([call('a', undefined, 'subagent'), call('b', undefined, 'subagent'), call('a1', 'a'), call('b1', 'b'), call('a2', 'a', 'subagent'), call('a2x', 'a2'), call('top')]))
    const tree = toolTree(view.blocks)
    const ids = (p: string) => tree.children.get(p)?.map((b) => b.toolId)
    expect(ids('a')).toEqual(['a1', 'a2'])
    expect(ids('b')).toEqual(['b1'])
    expect(ids('a2')).toEqual(['a2x'])
    expect(countCalls('a', tree)).toBe(3)
    const top = view.blocks.filter((b) => b.type === 'tool' && !tree.nested.has(b.key)).map((b) => (b as ToolBlock).toolId)
    expect(top).toEqual(['a', 'b', 'top'])
  })

  it('keeps a call flat when its parent is not loaded, or points at itself', () => {
    const view = buildView(evs([call('x', 'gone'), call('self', 'self')]))
    const tree = toolTree(view.blocks)
    expect(tree.nested.size).toBe(0)
  })
})

describe('subagentState', () => {
  it('a backgrounded agent is still going until its final result replaces the launch result', () => {
    const launched = buildView(evs([call('a', undefined, 'subagent'), { type: 'tool_result', id: 'a', output: 'launched', is_error: false, detail: { background: true } }]))
    expect(subagentState(launched.blocks[0] as ToolBlock)).toBe('background')
    const finished = buildView(
      evs([
        call('a', undefined, 'subagent'),
        { type: 'tool_result', id: 'a', output: 'launched', is_error: false, detail: { background: true } },
        { type: 'tool_result', id: 'a', output: 'summary', is_error: true },
      ]),
    )
    expect(subagentState(finished.blocks[0] as ToolBlock)).toBe('error')
    expect(subagentState(buildView(evs([call('a', undefined, 'subagent')])).blocks[0] as ToolBlock)).toBe('running')
  })
})

describe('subagentRows', () => {
  const result = (id: string, detail?: { background?: boolean; total_tokens?: number }, is_error = false) => ({ type: 'tool_result', id, output: 'x', is_error, ...(detail ? { detail } : {}) }) as const
  const rows = (status: Parameters<typeof subagentRows>[1], events: Parameters<typeof evs>[0]) =>
    subagentRows(buildView(evs(events)).blocks, status).map((r) => [r.block.toolId, r.state, r.depth, r.calls, r.tokens ?? null])

  it('lists agents parent-first with depth, nested calls and reported tokens', () => {
    const events = [
      call('a', undefined, 'subagent'),
      call('b', undefined, 'subagent'),
      call('a1', 'a'),
      call('a2', 'a', 'subagent'),
      call('a2x', 'a2'),
      result('a2', { total_tokens: 900 }),
      result('a', { total_tokens: 4200 }),
      call('top'),
    ] as const
    expect(rows('running', [...events])).toEqual([
      ['a', 'done', 0, 3, 4200],
      ['a2', 'done', 1, 1, 900],
      ['b', 'running', 0, 0, null],
    ])
  })

  it('an agent without a result is stopped once its turn or its parent agent is over', () => {
    const events = [call('a', undefined, 'subagent'), call('b', undefined, 'subagent'), call('b1', 'b', 'subagent'), result('b', { background: true }), call('c', undefined, 'subagent'), call('c1', 'c', 'subagent'), result('c')] as const
    // turn 结束：a 不会再有结果；b 在后台、它启动的 b1 跟着还在跑；c 结束了，它下面没结果的 c1 也停了
    expect(rows('idle', [...events]).map(([id, state]) => [id, state])).toEqual([
      ['a', 'stopped'],
      ['b', 'background'],
      ['b1', 'running'],
      ['c', 'done'],
      ['c1', 'stopped'],
    ])
    // 进程没了：后台的也停了
    expect(rows('ended', [...events]).map(([id, state]) => [id, state])).toEqual([
      ['a', 'stopped'],
      ['b', 'stopped'],
      ['b1', 'stopped'],
      ['c', 'done'],
      ['c1', 'stopped'],
    ])
  })
})
