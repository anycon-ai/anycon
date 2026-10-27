// 子 agent 的调用按 parent_id 归组：只认排在父调用之后的，计数含嵌套，后台子 agent 等补的结果才算完。
import { beforeEach, describe, expect, it } from 'vitest'
import type { Event } from '@/protocol'
import { evs, resetSeq } from '@/test/factories'
import { buildView, type ToolBlock } from './reducer'
import { backgroundCommands, countCalls, subagentRows, subagentState, toolTree } from './subagents'

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

describe('backgroundCommands', () => {
  const launched = (id: string) => ({ type: 'tool_result', id, output: 'Command running in background with ID: b1.', is_error: false, detail: { background: true } }) as const
  it('lists the commands still running in the background until their final result arrives; agents are not commands', () => {
    const events = [call('bg'), launched('bg'), call('fg'), { type: 'tool_result', id: 'fg', output: '', is_error: false, detail: { exit_code: 0 } }, call('agent', undefined, 'subagent'), launched('agent')] as const
    const ids = (status: Parameters<typeof backgroundCommands>[1], more: readonly object[] = []) =>
      backgroundCommands(buildView(evs([...events, ...more] as never)).blocks, status).map((b) => b.toolId)
    expect(ids('idle')).toEqual(['bg'])
    expect(ids('running')).toEqual(['bg'])
    // 结束时补的结果不带 background
    expect(ids('idle', [{ type: 'tool_result', id: 'bg', output: 'Background command "x" completed (exit code 0)', is_error: false, detail: { exit_code: 0 } }])).toEqual([])
    // 进程不在了：后台命令也跟着没了
    expect(ids('ended')).toEqual([])
    expect(ids('interrupted')).toEqual([])
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

// 事件形状取自真实录制：Claude 的 Workflow 调用结果是「Workflow launched in background. Task ID: …」、不带 detail；
// Codex 的 spawnAgent（来自 subAgentActivity）紧跟着立刻返回的 tool_result，子 agent 的状态只在 task 快照里。
describe('state of a call with tasks attached', () => {
  const workflow: Event[] = [
    { type: 'tool_call', id: 'toolu_wf', name: 'Workflow', category: 'other', title: 'Run workflow: alpha-beta', input: { script: 'x' } },
    { type: 'tool_result', id: 'toolu_wf', output: 'Workflow launched in background. Task ID: whvf25gph', is_error: false },
  ]
  const spawn: Event[] = [
    { type: 'tool_call', id: 'call_1', name: 'spawnAgent', category: 'subagent', title: 'Start sub-agent pong', input: { agentThreadId: 'child' } },
    { type: 'tool_result', id: 'call_1', output: 'Started sub-agent pong (fixture-model-sub)', is_error: false },
  ]
  const task = (id: string, tool_id: string, status: 'running' | 'completed' | 'failed' | 'cancelled', kind: 'workflow' | 'subagent' = 'workflow'): Event => ({ type: 'task', id, kind, tool_id, title: id, status, phases: [], agents: [] })
  const only = (events: Event[]) => buildView(evs(events)).blocks.find((b): b is ToolBlock => b.type === 'tool')!

  it('a launch result without the background flag is still background while its task runs, then done, failed or cancelled with it', () => {
    expect(only([...workflow, task('w', 'toolu_wf', 'running')]).result?.detail).toBeFalsy()
    expect(subagentState(only([...workflow, task('w', 'toolu_wf', 'running')]))).toBe('background')
    expect(subagentState(only([...workflow, task('w', 'toolu_wf', 'running'), task('w', 'toolu_wf', 'completed')]))).toBe('done')
    expect(subagentState(only([...workflow, task('w', 'toolu_wf', 'failed')]))).toBe('error')
    expect(subagentState(only([...workflow, task('w', 'toolu_wf', 'cancelled')]))).toBe('done')
    // 没有任务时照旧：没有 background 标记的结果就是完成
    expect(subagentState(only(workflow))).toBe('done')
  })

  it('several tasks on one call: any running one keeps it in the background, a failed one makes it an error once nothing runs', () => {
    expect(subagentState(only([...workflow, task('a', 'toolu_wf', 'completed'), task('b', 'toolu_wf', 'running')]))).toBe('background')
    expect(subagentState(only([...workflow, task('a', 'toolu_wf', 'failed'), task('b', 'toolu_wf', 'running')]))).toBe('background')
    expect(subagentState(only([...workflow, task('a', 'toolu_wf', 'failed'), task('b', 'toolu_wf', 'completed')]))).toBe('error')
    expect(subagentState(only([...workflow, task('a', 'toolu_wf', 'cancelled'), task('b', 'toolu_wf', 'completed')]))).toBe('done')
  })

  it('before the call returns it is running whatever the task says; an error result stays an error', () => {
    expect(subagentState(only([workflow[0]!, task('w', 'toolu_wf', 'running')]))).toBe('running')
    expect(subagentState(only([workflow[0]!, { type: 'tool_result', id: 'toolu_wf', output: 'boom', is_error: true }, task('w', 'toolu_wf', 'running')]))).toBe('error')
  })

  it('a running task nobody can advance any more (live = false) is stopped, not running', () => {
    const block = only([...workflow, task('w', 'toolu_wf', 'running')])
    expect(subagentState(block, true)).toBe('background')
    expect(subagentState(block, false)).toBe('stopped')
    // 终态的任务不看 live
    expect(subagentState(only([...workflow, task('w', 'toolu_wf', 'completed')]), false)).toBe('done')
  })

  it('a task that goes back from completed to running (the sub-agent got new input) makes the call a background one again', () => {
    const events = [...spawn, task('child', 'call_1', 'running', 'subagent'), task('child', 'call_1', 'completed', 'subagent')]
    expect(subagentState(only(events))).toBe('done')
    expect(subagentState(only([...events, task('child', 'call_1', 'running', 'subagent')]))).toBe('background')
    expect(subagentState(only([...events, task('child', 'call_1', 'running', 'subagent'), task('child', 'call_1', 'completed', 'subagent')]))).toBe('done')
  })

  it('subagentRows: a spawnAgent with a running task is a background row while the main agent is idle, stopped once nothing is live', () => {
    const view = buildView(evs([...spawn, task('child', 'call_1', 'running', 'subagent')]))
    const state = (status: Parameters<typeof subagentRows>[1], live?: boolean) => subagentRows(view.blocks, status, live).map((r) => r.state)
    // 主 turn 结束了，但 daemon 数到子 agent 还在跑（live = tasksLive(status, background_tasks) = true）
    expect(state('idle', true)).toEqual(['background'])
    // 没人会再推进它
    expect(state('idle', false)).toEqual(['stopped'])
    expect(state('idle')).toEqual(['stopped'])
    // 主 turn 还在跑：默认 live
    expect(state('running')).toEqual(['background'])
    // 进程没了
    expect(state('ended', true)).toEqual(['stopped'])
    const finished = buildView(evs([...spawn, task('child', 'call_1', 'running', 'subagent'), task('child', 'call_1', 'completed', 'subagent')]))
    expect(subagentRows(finished.blocks, 'idle', false).map((r) => r.state)).toEqual(['done'])
    const failed = buildView(evs([...spawn, task('child', 'call_1', 'failed', 'subagent')]))
    expect(subagentRows(failed.blocks, 'idle', false).map((r) => r.state)).toEqual(['error'])
  })

  it('backgroundCommands lists any non-agent call with a running task (a Workflow), and drops it when the task ends, stops or the process is gone', () => {
    const running = buildView(evs([...workflow, task('w', 'toolu_wf', 'running')])).blocks
    const ids = (blocks: typeof running, status: Parameters<typeof backgroundCommands>[1], live?: boolean) => backgroundCommands(blocks, status, live).map((b) => b.toolId)
    expect(ids(running, 'idle')).toEqual(['toolu_wf'])
    expect(ids(running, 'idle', false)).toEqual([])
    expect(ids(running, 'ended')).toEqual([])
    expect(ids(buildView(evs([...workflow, task('w', 'toolu_wf', 'completed')])).blocks, 'idle')).toEqual([])
    // 子 agent 不在这里（在 subagentRows 里）
    expect(ids(buildView(evs([...spawn, task('child', 'call_1', 'running', 'subagent')])).blocks, 'idle')).toEqual([])
  })

  it('a background command whose task has finished is no longer listed even if the flagged launch result was never replaced', () => {
    const cmd: Event[] = [
      { type: 'tool_call', id: 'b', name: 'Bash', category: 'shell', title: 'sleep 5', input: { command: 'sleep 5', run_in_background: true } },
      { type: 'tool_result', id: 'b', output: 'Command running in background with ID: b1.', is_error: false, detail: { background: true } },
    ]
    expect(backgroundCommands(buildView(evs([...cmd, task('b1', 'b', 'running')])).blocks, 'idle').map((b) => b.toolId)).toEqual(['b'])
    expect(backgroundCommands(buildView(evs([...cmd, task('b1', 'b', 'completed')])).blocks, 'idle')).toEqual([])
  })
})

