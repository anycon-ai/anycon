import { beforeEach, describe, expect, it } from 'vitest'
import { ev, evs, resetSeq } from '@/test/factories'
import { applyEvents, buildView, diffSettings, emptyView, latestSyncBlock, pendingPermissions, planIngest, type AssistantBlock, type GitSystemBlock, type PermissionBlock, type ToolBlock } from './reducer'

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
