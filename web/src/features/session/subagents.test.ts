// 子 agent 的调用按 parent_id 归组：只认排在父调用之后的，计数含嵌套，后台子 agent 等补的结果才算完。
import { beforeEach, describe, expect, it } from 'vitest'
import { evs, resetSeq } from '@/test/factories'
import { buildView, type ToolBlock } from './reducer'
import { countCalls, subagentState, toolTree } from './subagents'

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
