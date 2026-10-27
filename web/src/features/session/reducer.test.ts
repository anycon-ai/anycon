import { beforeEach, describe, expect, it } from 'vitest'
import { ev, evs, resetSeq } from '@/test/factories'
import { applyEvents, buildView, diffSettings, emptyView, latestSyncBlock, pendingPermissions, planIngest, type AssistantBlock, type GitSystemBlock, type NoticeBlock, type PermissionBlock, type TaskBlock, type ToolBlock, type TurnStartBlock } from './reducer'
import type { Event } from '@/protocol'

beforeEach(() => resetSeq())

describe('session event reducer', () => {
  it('coalesces text deltas into one assistant message', () => {
    const v = buildView(evs([
      { type: 'turn_start', turn_id: 1, message_id: 'm1' },
      { type: 'text_delta', text: 'Hel' },
      { type: 'text_delta', text: 'lo ' },
      { type: 'text_delta', text: 'world' },
      { type: 'text_done' },
    ]))
    const assistants = v.blocks.filter((b): b is AssistantBlock => b.type === 'assistant')
    expect(assistants).toHaveLength(1)
    expect(assistants[0]!.text).toBe('Hello world')
    expect(assistants[0]!.done).toBe(true)
    expect(v.lastSeq).toBe(5)
  })

  it('opens a new assistant block after a tool call interrupts the text', () => {
    const v = buildView(evs([
      { type: 'text_delta', text: 'before' },
      { type: 'tool_call', id: 't1', name: 'Bash', category: 'shell', title: 'echo', input: {} },
      { type: 'text_delta', text: 'after' },
    ]))
    expect(v.blocks.map((b) => b.type)).toEqual(['assistant', 'tool', 'assistant'])
    expect((v.blocks[0] as AssistantBlock).done).toBe(true)
  })

  it('merges thinking deltas and records duration on thinking_done', () => {
    const v = buildView(evs([
      { type: 'thinking_delta', text: 'a' },
      { type: 'thinking_delta', text: '' },
      { type: 'thinking_delta', text: 'b' },
      { type: 'thinking_done', duration_ms: 12_000 },
      { type: 'text_delta', text: 'ok' },
    ]))
    expect(v.blocks[0]).toMatchObject({ type: 'thinking', text: 'ab', done: true, durationMs: 12_000 })
    expect(v.blocks[1]!.type).toBe('assistant')
  })

  it('attaches streamed output and the result to the tool block', () => {
    const v = buildView(evs([
      { type: 'tool_call', id: 't1', name: 'Bash', category: 'shell', title: 'echo hi', input: { command: 'echo hi' }, detail: { command: 'echo hi' } },
      { type: 'tool_output_delta', id: 't1', text: 'h' },
      { type: 'tool_output_delta', id: 't1', text: 'i\n' },
      { type: 'tool_result', id: 't1', output: 'hi\n', is_error: false, detail: { exit_code: 0 } },
    ]))
    expect(v.blocks).toHaveLength(1)
    const t = v.blocks[0] as ToolBlock
    expect(t.streamed).toBe('hi\n')
    expect(t.result).toMatchObject({ isError: false, detail: { exit_code: 0 } })
  })

  it('marks a failed tool result', () => {
    const v = buildView(evs([
      { type: 'tool_call', id: 't1', name: 'Bash', category: 'shell', title: 'x', input: {} },
      { type: 'tool_result', id: 't1', output: 'boom', is_error: true },
    ]))
    expect((v.blocks[0] as ToolBlock).result?.isError).toBe(true)
  })

  it('creates a placeholder tool block when only the result is in the window', () => {
    const v = buildView(evs([{ type: 'tool_result', id: 'orphan', output: 'x', is_error: false }]))
    expect(v.blocks[0]).toMatchObject({ type: 'tool', toolId: 'orphan' })
  })

  it('updates the plan card in place within a turn', () => {
    const v = buildView(evs([
      { type: 'turn_start', turn_id: 1, message_id: 'm' },
      { type: 'plan', entries: [{ content: 'a', status: 'pending' }] },
      { type: 'text_delta', text: 'working' },
      { type: 'plan', entries: [{ content: 'a', status: 'completed' }] },
    ]))
    const plans = v.blocks.filter((b) => b.type === 'plan')
    expect(plans).toHaveLength(1)
    expect(plans[0]).toMatchObject({ entries: [{ content: 'a', status: 'completed' }] })
  })

  it('starts a new plan card in the next turn', () => {
    const v = buildView(evs([
      { type: 'turn_start', turn_id: 1, message_id: 'm' },
      { type: 'plan', entries: [{ content: 'a', status: 'pending' }] },
      { type: 'turn_done', turn_id: 1, reason: 'completed' },
      { type: 'turn_start', turn_id: 2, message_id: 'm2' },
      { type: 'plan', entries: [{ content: 'b', status: 'pending' }] },
    ]))
    expect(v.blocks.filter((b) => b.type === 'plan')).toHaveLength(2)
  })

  it('updates the permission card when it is resolved', () => {
    const base = evs([
      {
        type: 'permission',
        request_id: 'r1',
        kind: 'tool',
        title: 'Run command',
        detail: {},
        options: [{ id: 'allow', label: 'Allow', kind: 'allow_once' }],
      },
    ])
    const v1 = buildView(base)
    expect(pendingPermissions(v1)).toHaveLength(1)
    const v2 = applyEvents(v1, [ev({ type: 'permission_resolved', request_id: 'r1', option_id: 'allow', by_device: 'dev1' })])
    expect(pendingPermissions(v2)).toHaveLength(0)
    expect((v2.blocks[0] as PermissionBlock).resolved).toMatchObject({ optionId: 'allow', byDevice: 'dev1' })
    // 旧视图不可变
    expect((v1.blocks[0] as PermissionBlock).resolved).toBeUndefined()
  })

  it('keeps the answers and the note the device sent with its reply, dropping values that are not text', () => {
    const v = buildView(
      evs([
        { type: 'permission', request_id: 'r1', kind: 'question', title: 'Agent has a question', detail: { questions: [{ id: 'q1', text: 'Which?' }] }, options: [] },
        { type: 'permission_resolved', request_id: 'r1', by_device: 'dev1', answers: { q1: 'SQLite', q2: ['Auth', 3, 'Billing'], q3: 7 }, message: '  thanks  ' },
      ]),
    )
    expect((v.blocks[0] as PermissionBlock).resolved).toEqual({ optionId: null, byDevice: 'dev1', ts: expect.any(Number), answers: { q1: 'SQLite', q2: ['Auth', 'Billing'] }, message: 'thanks' })
    // 旧事件和 daemon 自己答复的不带：没有这两项
    const old = buildView(evs([{ type: 'permission', request_id: 'r2', kind: 'question', title: 't', detail: {}, options: [] }, { type: 'permission_resolved', request_id: 'r2', by_device: 'daemon', answers: { q1: 1 }, message: ' ' }]))
    expect((old.blocks[0] as PermissionBlock).resolved).not.toHaveProperty('answers')
    expect((old.blocks[0] as PermissionBlock).resolved).not.toHaveProperty('message')
  })

  it('ignores resolution for an unknown request', () => {
    const v = buildView(evs([{ type: 'permission_resolved', request_id: 'nope', option_id: 'x', by_device: 'd' }]))
    expect(v.blocks).toHaveLength(0)
  })

  it('collects usage from turn_done', () => {
    const v = buildView(evs([
      { type: 'turn_done', turn_id: 1, reason: 'completed', usage: { input_tokens: 10, output_tokens: 5, cost_usd: 0.01, duration_ms: 1200, model: 'm' } },
      { type: 'turn_done', turn_id: 2, reason: 'completed', usage: { input_tokens: 1, output_tokens: 2, duration_ms: 10 } },
    ]))
    expect(v.usage).toMatchObject({ turns: 2, inputTokens: 11, outputTokens: 7, costUsd: 0.01 })
    expect(v.blocks.filter((b) => b.type === 'turn_done')).toHaveLength(2)
  })

  it('tracks status, title and changed files', () => {
    const v = buildView(evs([
      { type: 'status', state: 'running', queued_messages: 1 },
      { type: 'title_changed', title: 'T' },
      { type: 'file_changed', path: 'a.ts', kind: 'created' },
      { type: 'file_changed', path: 'a.ts', kind: 'modified' },
      { type: 'file_changed', path: 'b.ts', kind: 'deleted' },
    ]))
    expect(v.status).toBe('running')
    expect(v.queuedMessages).toBe(1)
    expect(v.title).toBe('T')
    expect(v.changedFiles).toEqual({ 'a.ts': 'created', 'b.ts': 'deleted' })
  })

  it('renders settings_changed as a system line with a diff against the previous settings', () => {
    const v0 = emptyView({ model: 'mock-large', thinking: 'high', mode: 'ask' })
    const v = applyEvents(v0, [ev({ type: 'settings_changed', settings: { model: 'mock-small', thinking: 'high', mode: 'ask' } })])
    const sys = v.blocks[0]
    expect(sys).toMatchObject({ type: 'system', changes: [{ field: 'model', from: 'mock-large', to: 'mock-small' }] })
  })

  it('marks the first settings_changed as initial when the previous settings are unknown', () => {
    const v = buildView(evs([{ type: 'settings_changed', settings: { model: 'm', mode: 'ask' } }]))
    expect(v.blocks[0]).toMatchObject({ type: 'system', initial: true })
    const v2 = buildView(evs([{ type: 'settings_changed', settings: { model: 'm2' } }]), { model: 'm' })
    expect(v2.blocks[0]).toMatchObject({ initial: false, changes: [{ field: 'model', from: 'm', to: 'm2' }] })
  })

  it('skips duplicates and out-of-date seqs', () => {
    const events = evs([
      { type: 'text_delta', text: 'a' },
      { type: 'text_delta', text: 'b' },
    ])
    const v1 = buildView(events)
    const v2 = applyEvents(v1, events)
    expect(v2).toBe(v1)
  })

  it('records errors', () => {
    const v = buildView(evs([{ type: 'error', code: 'agent_crashed', message: 'x', recoverable: true }]))
    expect(v.blocks[0]).toMatchObject({ type: 'error', recoverable: true })
  })

  it('does not duplicate a user message that appears twice', () => {
    const e = ev({ type: 'user_message', message_id: 'm1', content: 'hi', by_device: 'd' })
    const v = applyEvents(applyEvents(emptyView(), [e]), [{ ...e, seq: e.seq + 1 }])
    expect(v.blocks.filter((b) => b.type === 'user')).toHaveLength(1)
  })
})

describe('diffSettings', () => {
  it('lists option changes', () => {
    expect(diffSettings({ options: { verbose: false } }, { options: { verbose: true } })).toEqual([{ field: 'option', id: 'verbose', from: 'false', to: 'true' }])
  })
})

describe('planIngest', () => {
  const e = (seq: number) => ev({ type: 'text_delta', text: String(seq) }, { seq })
  it('accepts a contiguous run', () => {
    const p = planIngest(5, [e(6), e(7)])
    expect(p.apply.map((x) => x.seq)).toEqual([6, 7])
    expect(p.gap).toBe(false)
  })
  it('flags a gap and holds back events after it', () => {
    const p = planIngest(5, [e(6), e(8), e(9)])
    expect(p.apply.map((x) => x.seq)).toEqual([6])
    expect(p.ahead.map((x) => x.seq)).toEqual([8, 9])
    expect(p.gap).toBe(true)
  })
  it('drops already-seen events', () => {
    const p = planIngest(5, [e(3), e(5)])
    expect(p.apply).toEqual([])
    expect(p.gap).toBe(false)
  })
  it('accepts a permanent gap when asked (merged deltas never come back)', () => {
    const p = planIngest(5, [e(8), e(9), e(9)], { allowGap: true })
    expect(p.apply.map((x) => x.seq)).toEqual([8, 9])
    expect(p.gap).toBe(false)
  })
  it('takes the first event as a base when nothing is known yet', () => {
    const p = planIngest(0, [e(40), e(41)])
    expect(p.apply.map((x) => x.seq)).toEqual([40, 41])
  })
})

describe('git events (ADR-34)', () => {
  const git = (b: unknown): GitSystemBlock => b as GitSystemBlock

  it('turns each git event into a system block that carries its fields, and closes the open text block', () => {
    const v = buildView(
      evs([
        { type: 'text_delta', text: 'working…' },
        { type: 'git', action: 'worktree_created', branch: 'anycon/x', base: 'main', count: 2 },
        { type: 'git', action: 'sync_conflicts', branch: 'anycon/x', base: 'main', files: ['a.rs', 'b.rs'] },
        { type: 'git', action: 'sync_aborted', branch: 'anycon/x', base: null, count: null, note: null },
      ]),
    )
    expect(v.blocks.map((b) => b.type)).toEqual(['assistant', 'system', 'system', 'system'])
    expect((v.blocks[0] as AssistantBlock).done).toBe(true)
    expect(git(v.blocks[1])).toMatchObject({ kind: 'git', action: 'worktree_created', branch: 'anycon/x', base: 'main', count: 2, files: [] })
    expect(git(v.blocks[2])).toMatchObject({ kind: 'git', action: 'sync_conflicts', files: ['a.rs', 'b.rs'] })
    // null 的可选字段规整成 undefined / 空数组，渲染层不用判 null
    expect(git(v.blocks[3])).toMatchObject({ base: undefined, count: undefined, note: undefined, files: [] })
  })

  it('does not disturb the settings_changed system line', () => {
    const v = buildView(evs([{ type: 'settings_changed', settings: { model: 'm' } }, { type: 'git', action: 'worktree_kept', branch: 'b' }]))
    expect(v.blocks.map((b) => (b.type === 'system' ? b.kind : b.type))).toEqual(['settings_changed', 'git'])
  })

  it('latestSyncBlock only looks at sync_* events: a later sync result ends an earlier conflict', () => {
    const v = buildView(
      evs([
        { type: 'git', action: 'worktree_created', branch: 'b', base: 'main' },
        { type: 'git', action: 'sync_conflicts', branch: 'b', base: 'main', files: ['a'] },
        { type: 'git', action: 'worktree_kept', branch: 'b' },
      ]),
    )
    expect(latestSyncBlock(v.blocks)?.action).toBe('sync_conflicts')
    const v2 = applyEvents(v, [ev({ type: 'git', action: 'sync_aborted', branch: 'b', base: 'main' })])
    expect(latestSyncBlock(v2.blocks)?.action).toBe('sync_aborted')
    expect(latestSyncBlock([])).toBeNull()
  })
})

describe('turn_start from the agent itself', () => {
  const turnStarts = (blocks: readonly { type: string }[]): TurnStartBlock[] => blocks.filter((b): b is TurnStartBlock => b.type === 'turn_start')

  it('marks a turn whose message_id is empty as started by the agent; a turn for a user message is not', () => {
    const v = buildView(
      evs([
        { type: 'user_message', message_id: 'm1', content: 'go', by_device: 'me' },
        { type: 'turn_start', turn_id: 1, message_id: 'm1' },
        { type: 'turn_done', turn_id: 1, reason: 'completed' },
        // 后台任务结束后 CLI 自己再跑的一轮：没有对应的用户消息
        { type: 'turn_start', turn_id: 2, message_id: '' },
        { type: 'text_delta', text: 'The background task finished.' },
        { type: 'turn_done', turn_id: 2, reason: 'completed' },
      ]),
    )
    expect(turnStarts(v.blocks).map((b) => [b.turnId, b.agentStarted])).toEqual([
      [1, false],
      [2, true],
    ])
    // 其余块照常：这一轮的回复和 turn_done 都挂在 turn 2 上，没有凭空多出用户气泡
    expect(v.blocks.filter((b) => b.type === 'user')).toHaveLength(1)
    expect(v.blocks.filter((b) => b.type === 'assistant' && b.turnId === 2)).toHaveLength(1)
    expect(v.usage.turns).toBe(2)
  })

  it('applies incrementally the same way: an empty message_id arriving later still produces the marker', () => {
    const v1 = buildView(evs([{ type: 'turn_start', turn_id: 1, message_id: 'm1' }, { type: 'turn_done', turn_id: 1, reason: 'completed' }]))
    const v2 = applyEvents(v1, [ev({ type: 'turn_start', turn_id: 2, message_id: '' })])
    expect(turnStarts(v2.blocks).map((b) => b.agentStarted)).toEqual([false, true])
  })
})

describe('task snapshots', () => {
  const workflowCall = { type: 'tool_call', id: 'tool-1', name: 'Workflow', category: 'subagent', title: 'Workflow: review', input: {} } as const
  const snapshot = (over: Partial<Extract<Event, { type: 'task' }>> = {}): Event => ({ type: 'task', id: 'task-1', kind: 'workflow', tool_id: 'tool-1', title: 'review', status: 'running', ...over })

  it('attaches the snapshot to the tool block that started it, without adding a row', () => {
    const v = buildView(evs([workflowCall, snapshot({ usage: { total_tokens: 120 }, summary: 'Scanning' })]))
    expect(v.blocks.map((b) => b.type)).toEqual(['tool'])
    const tool = v.blocks[0] as ToolBlock
    expect(tool.tasks).toHaveLength(1)
    expect(tool.tasks![0]).toMatchObject({ id: 'task-1', kind: 'workflow', toolId: 'tool-1', title: 'review', status: 'running', usage: { total_tokens: 120 }, summary: 'Scanning', phases: [], agents: [] })
  })

  it('replaces an earlier snapshot of the same id as a whole, in place', () => {
    const v = buildView(
      evs([
        workflowCall,
        snapshot({ phases: [{ index: 0, title: 'Scan' }], agents: [{ index: 0, label: 'scanner', state: 'running', phase_index: 0 }], last_tool: 'Read' }),
        { type: 'text_delta', text: 'working' },
        snapshot({ status: 'completed', agents: [{ index: 0, label: 'scanner', state: 'done', phase_index: 0 }] }),
      ]),
    )
    expect(v.blocks.map((b) => b.type)).toEqual(['tool', 'assistant'])
    const tasks = (v.blocks[0] as ToolBlock).tasks!
    expect(tasks).toHaveLength(1)
    // 整条取代，不是合并：第二条没带 phases / last_tool，就都没有了
    expect(tasks[0]).toMatchObject({ status: 'completed', phases: [], lastTool: undefined, agents: [{ label: 'scanner', state: 'done' }] })
    expect(tasks[0]!.seq).toBe(4)
  })

  it('keeps several tasks started by one tool call in order of first appearance', () => {
    const v = buildView(evs([workflowCall, snapshot({ id: 'a', title: 'A' }), snapshot({ id: 'b', title: 'B' }), snapshot({ id: 'a', title: 'A', status: 'completed' })]))
    const tasks = (v.blocks[0] as ToolBlock).tasks!
    expect(tasks.map((t) => [t.id, t.status])).toEqual([
      ['a', 'completed'],
      ['b', 'running'],
    ])
  })

  it('makes a standalone block at the first appearance when tool_id is missing or unknown, and updates it in place', () => {
    const v = buildView(
      evs([
        { type: 'text_delta', text: 'before' },
        snapshot({ id: 'lonely', tool_id: 'not-loaded' }),
        { type: 'text_delta', text: 'after' },
        snapshot({ id: 'nobody', tool_id: null }),
        snapshot({ id: 'lonely', tool_id: 'not-loaded', status: 'failed' }),
      ]),
    )
    expect(v.blocks.map((b) => b.type)).toEqual(['assistant', 'task', 'assistant', 'task'])
    const [first, second] = v.blocks.filter((b): b is TaskBlock => b.type === 'task')
    expect(first!.task).toMatchObject({ id: 'lonely', status: 'failed' })
    expect(first!.key).toBe('task-lonely')
    expect(second!.task.id).toBe('nobody')
    // 第一次出现的位置先收口了前面的文字，后面的文字另起一块
    expect((v.blocks[0] as AssistantBlock).done).toBe(true)
  })

  it('does not move a standalone task onto a tool block that shows up later', () => {
    const v = buildView(evs([snapshot({ id: 'early' }), workflowCall, snapshot({ id: 'early', status: 'completed' })]))
    expect(v.blocks.map((b) => b.type)).toEqual(['task', 'tool'])
    expect((v.blocks[0] as TaskBlock).task.status).toBe('completed')
    expect((v.blocks[1] as ToolBlock).tasks).toBeUndefined()
  })

  it('keeps the attached tasks when the tool_call or tool_result for the block arrive again', () => {
    const v = buildView(evs([workflowCall, snapshot(), { ...workflowCall, title: 'Workflow: review (again)' }, { type: 'tool_result', id: 'tool-1', output: 'done', is_error: false }]))
    const tool = v.blocks[0] as ToolBlock
    expect(tool.title).toBe('Workflow: review (again)')
    expect(tool.result?.output).toBe('done')
    expect(tool.tasks).toHaveLength(1)
  })

  it('a completed task goes back to running when a later snapshot says so (the sub-agent got new input), in the same place', () => {
    const spawn = { type: 'tool_call', id: 'call_1', name: 'spawnAgent', category: 'subagent', title: 'Start sub-agent', input: {} } as const
    const child = (status: 'running' | 'completed'): Event => ({ type: 'task', id: 'child', kind: 'subagent', tool_id: 'call_1', title: 'pong', status })
    const v = buildView(evs([spawn, child('running'), child('completed'), { type: 'text_delta', text: 'the sub-agent said PONG' }, child('running')]))
    expect(v.blocks.map((b) => b.type)).toEqual(['tool', 'assistant'])
    expect((v.blocks[0] as ToolBlock).tasks!.map((t) => [t.id, t.status])).toEqual([['child', 'running']])
    const back = applyEvents(v, [ev(child('completed'))])
    expect((back.blocks[0] as ToolBlock).tasks![0]!.status).toBe('completed')
  })

  it('applies incrementally the same way as building from scratch', () => {
    const events = evs([workflowCall, snapshot(), snapshot({ status: 'completed' })])
    const inc = applyEvents(applyEvents(emptyView(), events.slice(0, 2)), events.slice(2))
    expect(inc.blocks).toEqual(buildView(events).blocks)
    expect(inc.taskIndex).toEqual({ 'task-1': 0 })
  })
})

describe('notice events', () => {
  it('makes a notice block with the level and the model / reset fields, closing the open text', () => {
    const v = buildView(
      evs([
        { type: 'text_delta', text: 'hi' },
        { type: 'notice', code: 'model_fallback', level: 'warning', message: 'Model A is overloaded', model_from: 'a', model_to: 'b' },
        { type: 'notice', code: 'rate_limit', level: 'warning', message: '', resets_at_ms: 1_700_000_000_000 },
        { type: 'notice', code: 'command_output', level: 'info', message: 'line 1\nline 2' },
      ]),
    )
    expect(v.blocks.map((b) => b.type)).toEqual(['assistant', 'notice', 'notice', 'notice'])
    expect((v.blocks[0] as AssistantBlock).done).toBe(true)
    const [fallback, limit, output] = v.blocks.slice(1) as NoticeBlock[]
    expect(fallback).toMatchObject({ code: 'model_fallback', level: 'warning', message: 'Model A is overloaded', modelFrom: 'a', modelTo: 'b' })
    expect(limit).toMatchObject({ code: 'rate_limit', resetsAtMs: 1_700_000_000_000, modelFrom: undefined })
    expect(output).toMatchObject({ code: 'command_output', level: 'info', message: 'line 1\nline 2' })
  })

  it('keeps unknown codes as they are', () => {
    const v = buildView(evs([{ type: 'notice', code: 'something_new', level: 'info', message: 'hello' }]))
    expect(v.blocks[0]).toMatchObject({ type: 'notice', code: 'something_new', message: 'hello' })
  })
})

describe('activity', () => {
  const retrying = { type: 'activity', state: 'retrying', attempt: 2, max_attempts: 10, delay_ms: 5000, reason: '529 overloaded_error' } as const

  it('records the transient state at the time of the event and adds no block', () => {
    const v = buildView(evs([{ type: 'turn_start', turn_id: 1, message_id: 'm' }, retrying]))
    expect(v.blocks.map((b) => b.type)).toEqual(['turn_start'])
    expect(v.activity).toEqual({ state: 'retrying', attempt: 2, maxAttempts: 10, delayMs: 5000, reason: '529 overloaded_error', seq: 2, ts: 1_002 })
    const compacting = applyEvents(v, [ev({ type: 'activity', state: 'compacting' })])
    expect(compacting.activity).toMatchObject({ state: 'compacting', attempt: undefined, delayMs: undefined })
  })

  it('is cleared by state none', () => {
    const v = buildView(evs([retrying, { type: 'activity', state: 'none' }]))
    expect(v.activity).toBeNull()
  })

  it.each<[string, Event]>([
    ['text', { type: 'text_delta', text: 'x' }],
    ['thinking', { type: 'thinking_delta', text: 'x' }],
    ['a tool call', { type: 'tool_call', id: 't', name: 'Bash', category: 'shell', title: 'x', input: {} }],
    ['a tool result', { type: 'tool_result', id: 't', output: '', is_error: false }],
    ['tool output', { type: 'tool_output_delta', id: 't', text: 'x' }],
    ['turn_done', { type: 'turn_done', turn_id: 1, reason: 'completed' }],
    ['an error', { type: 'error', code: 'x', message: 'x', recoverable: false }],
    ['the session going idle', { type: 'status', state: 'idle' }],
  ])('is cleared by %s', (_name, event) => {
    expect(buildView(evs([retrying, event])).activity).toBeNull()
  })

  it('survives events that are not content: notices, task snapshots, status running, file changes', () => {
    const v = buildView(
      evs([
        retrying,
        { type: 'status', state: 'running' },
        { type: 'notice', code: 'context_compacted', level: 'info', message: '' },
        { type: 'task', id: 't', kind: 'subagent', title: 'x', status: 'running' },
        { type: 'file_changed', path: 'a', kind: 'created' },
      ]),
    )
    expect(v.activity).toMatchObject({ state: 'retrying' })
  })
})

describe('context usage', () => {
  const done = (usage: Record<string, number>) => ({ type: 'turn_done', turn_id: 1, reason: 'completed', usage: { duration_ms: 1, ...usage } }) as const

  it('takes the latest context_window / context_tokens from turn_done', () => {
    const v = buildView(evs([done({ context_window: 200_000, context_tokens: 12_000 }), done({ context_window: 200_000, context_tokens: 46_000 })]))
    expect(v.context).toEqual({ window: 200_000, tokens: 46_000 })
  })

  it('needs both numbers, and a later turn without them does not erase the last known value', () => {
    expect(buildView(evs([done({ context_window: 200_000 })])).context).toBeNull()
    const v = buildView(evs([done({ context_window: 200_000, context_tokens: 12_000 }), done({ input_tokens: 5 }), { type: 'turn_done', turn_id: 3, reason: 'cancelled' }]))
    expect(v.context).toEqual({ window: 200_000, tokens: 12_000 })
  })
})
