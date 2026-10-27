// 信息面板的「Agent」卡片：主 agent 的状态和 token，子 agent 的状态、token、调用数，在工作的个数；不再有选项和改动的文件。
import { cleanup, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it } from 'vitest'
import type { Event, SessionStatus } from '@/protocol'
import { evs, resetSeq } from '@/test/factories'
import { buildView } from '../reducer'
import { backgroundCommands, subagentRows } from '../subagents'
import { InfoPanel, type InfoPanelProps } from './InfoPanel'

beforeEach(() => resetSeq())

const agent = (id: string, parent_id?: string): Event => ({ type: 'tool_call', id, name: 'Agent', category: 'subagent', title: `agent ${id}`, input: {}, ...(parent_id ? { parent_id } : {}) })
const bash = (id: string, parent_id: string): Event => ({ type: 'tool_call', id, name: 'Bash', category: 'shell', title: id, input: {}, parent_id })
const done = (id: string, total_tokens?: number): Event => ({ type: 'tool_result', id, output: 'ok', is_error: false, ...(total_tokens ? { detail: { total_tokens } } : {}) })

function panel(status: SessionStatus, events: Event[], over: Partial<InfoPanelProps> = {}) {
  const view = buildView(evs(events))
  const props: InfoPanelProps = {
    agentLabel: 'Claude Code',
    status,
    subagents: subagentRows(view.blocks, status),
    backgroundCommands: backgroundCommands(view.blocks, status),
    createdAt: 1,
    usage: { turns: 2, inputTokens: 12_300, outputTokens: 4_500, cacheReadTokens: 0, cacheWriteTokens: 0, costUsd: 0.5 },
    pending: [],
    onFocusPending: () => {},
    archived: false,
    onRename: () => {},
    onArchive: () => {},
    onDelete: () => {},
    onExport: () => {},
    onRestart: () => {},
    ...over,
  }
  return render(<InfoPanel {...props} />)
}

describe('InfoPanel agents card', () => {
  it('shows the main agent and each subagent with state, tokens and calls, and counts who is working', () => {
    panel('running', [agent('a'), bash('a1', 'a'), agent('a2', 'a'), done('a2', 900), done('a', 14_275), agent('b'), bash('b1', 'b')])
    const card = screen.getByTestId('info-agents')
    expect(card).toHaveTextContent('2 working')

    const main = screen.getByTestId('info-agent-main')
    expect(main).toHaveTextContent('Claude Code')
    expect(main).toHaveTextContent('12k in / 4.5k out')
    expect(main).toHaveTextContent('Running')

    const rows = screen.getAllByTestId('info-subagent')
    expect(rows.map((r) => r.dataset.state)).toEqual(['done', 'done', 'running'])
    expect(rows[0]).toHaveTextContent('agent a')
    expect(rows[0]).toHaveTextContent('14k tokens · 2 tool calls')
    expect(rows[1]).toHaveTextContent('900 tokens')
    expect(rows[1]).not.toHaveTextContent('tool call')
    expect(within(rows[2]!).getByText('Running')).toBeInTheDocument()
    expect(rows[2]).toHaveTextContent('1 tool call')
    expect(rows[2]).not.toHaveTextContent('tokens')
  })

  it('lists background commands while they run, and counts them as working even when the main agent is idle', () => {
    const cmd = (id: string, input: Record<string, string>): Event => ({ type: 'tool_call', id, name: 'Bash', category: 'shell', title: input.command ?? id, input })
    const launched = (id: string): Event => ({ type: 'tool_result', id, output: 'Command running in background with ID: b1.', is_error: false, detail: { background: true } })
    panel('idle', [cmd('t', { command: 'cargo test -p anycon-daemon', description: 'Run daemon tests' }), launched('t'), cmd('w', { command: 'bun run dev' }), launched('w'), agent('a'), done('a')], { backgroundTasks: 2 })
    expect(screen.getByTestId('info-agents')).toHaveTextContent('2 working')
    expect(screen.getByTestId('info-agent-main')).toHaveTextContent('Idle')
    const rows = screen.getAllByTestId('info-background-command')
    expect(rows.map((r) => r.textContent)).toEqual([expect.stringContaining('Run daemon tests'), expect.stringContaining('$ bun run dev')])
    for (const r of rows) expect(within(r).getByText('Background')).toBeInTheDocument()
    // daemon 数到的不比认出来的多：没有「另外 N 个」
    expect(screen.queryByTestId('info-background-other')).toBeNull()
  })

  it('shows the daemon’s background count for tasks it cannot match to a loaded call', () => {
    const { unmount } = panel('idle', [], { backgroundTasks: 2 })
    expect(screen.getByTestId('info-background-other')).toHaveTextContent('2 background tasks')
    expect(screen.getByTestId('info-agents')).toHaveTextContent('2 working')
    unmount()
    // 认出来一个后台子 agent：多出来的那个写成「另外 1 个」
    panel('idle', [agent('bg'), { type: 'tool_result', id: 'bg', output: 'launched', is_error: false, detail: { background: true } }], { backgroundTasks: 2 })
    expect(screen.getByTestId('info-background-other')).toHaveTextContent('1 more background task')
    expect(screen.getByTestId('info-agents')).toHaveTextContent('2 working')
  })

  it('a sub-agent whose launch result came back at once is a background agent while its task runs, and done after; the daemon count is not added on top', () => {
    const spawn: Event = { type: 'tool_call', id: 'call_1', name: 'spawnAgent', category: 'subagent', title: 'Start sub-agent pong', input: {} }
    const started: Event = { type: 'tool_result', id: 'call_1', output: 'Started sub-agent pong (fixture-model-sub)', is_error: false }
    const child = (status: 'running' | 'completed'): Event => ({ type: 'task', id: 'child', kind: 'subagent', tool_id: 'call_1', title: 'pong', status })
    const live = (status: SessionStatus, events: Event[]): Partial<InfoPanelProps> => {
      const view = buildView(evs(events))
      return { subagents: subagentRows(view.blocks, status, true), backgroundCommands: backgroundCommands(view.blocks, status, true) }
    }
    const events = [spawn, started, child('running')]
    const { unmount } = panel('idle', events, { ...live('idle', events), backgroundTasks: 1 })
    expect(screen.getByTestId('info-subagent')).toHaveAttribute('data-state', 'background')
    expect(within(screen.getByTestId('info-subagent')).getByText('Background')).toBeInTheDocument()
    expect(screen.getByTestId('info-agents')).toHaveTextContent('1 working')
    expect(screen.queryByTestId('info-background-other')).toBeNull()
    unmount()
    const finished = [...events, child('completed')]
    panel('idle', finished, { ...live('idle', finished), backgroundTasks: 0 })
    expect(screen.getByTestId('info-subagent')).toHaveAttribute('data-state', 'done')
    expect(screen.getByTestId('info-agents')).not.toHaveTextContent('working')
  })

  it('lists a Workflow call as a background item while its task runs (its launch result is not marked background)', () => {
    const events: Event[] = [
      { type: 'tool_call', id: 'toolu_wf', name: 'Workflow', category: 'other', title: 'Run workflow: alpha-beta', input: {} },
      { type: 'tool_result', id: 'toolu_wf', output: 'Workflow launched in background. Task ID: whvf25gph', is_error: false },
      { type: 'task', id: 'whvf25gph', kind: 'workflow', tool_id: 'toolu_wf', title: 'alpha-beta', status: 'running' },
    ]
    const view = buildView(evs(events))
    panel('idle', events, { backgroundCommands: backgroundCommands(view.blocks, 'idle', true), backgroundTasks: 1 })
    const rows = screen.getAllByTestId('info-background-command')
    expect(rows).toHaveLength(1)
    expect(rows[0]).toHaveTextContent('Run workflow: alpha-beta')
    expect(screen.getByTestId('info-agents')).toHaveTextContent('1 working')
    expect(screen.queryByTestId('info-background-other')).toBeNull()
  })

  it('has no options or changed-files cards any more', () => {
    panel('idle', [{ type: 'file_changed', path: 'src/a.ts', kind: 'created' }])
    expect(screen.queryByTestId('info-files')).toBeNull()
    expect(screen.queryByText('src/a.ts')).toBeNull()
    expect(screen.queryByText('Options')).toBeNull()
    expect(screen.queryByTestId('info-subagent')).toBeNull()
    expect(screen.getByTestId('info-agents')).not.toHaveTextContent('working')
  })

  it('keeps long lists short: running agents and the latest few stay, the rest expand on demand', async () => {
    const events: Event[] = [agent('bg'), { type: 'tool_result', id: 'bg', output: 'launched', is_error: false, detail: { background: true } }]
    for (let i = 0; i < 8; i++) events.push(agent(`s${i}`), done(`s${i}`))
    panel('idle', events)
    expect(screen.getByTestId('info-agents')).toHaveTextContent('1 working')
    const ids = () => screen.getAllByTestId('info-subagent').map((r) => r.textContent)
    // 后台那个还在跑，排在最前面也留着；完成的只留最近 6 个
    expect(ids()).toHaveLength(7)
    expect(ids()[0]).toContain('agent bg')
    expect(ids()[1]).toContain('agent s2')
    await userEvent.setup().click(screen.getByTestId('info-subagents-more'))
    expect(ids()).toHaveLength(9)
    expect(screen.queryByTestId('info-subagents-more')).toBeNull()
  })
})

describe('InfoPanel context usage', () => {
  it('shows a progress bar with the percentage and the numbers', () => {
    panel('idle', [], { context: { window: 200_000, tokens: 46_000 } })
    const row = screen.getByTestId('info-context')
    expect(row).toHaveTextContent('Context')
    expect(screen.getByTestId('info-context-value')).toHaveTextContent('23% · 46k / 200k')
    const bar = within(row).getByRole('progressbar', { name: 'Context' })
    expect(bar).toHaveAttribute('aria-valuenow', '23')
    expect(row).toHaveAttribute('data-percent', '23')
  })

  it('warns as the window fills up and never draws past the end', () => {
    const { unmount } = panel('idle', [], { context: { window: 200_000, tokens: 160_000 } })
    expect(within(screen.getByTestId('info-context')).getByRole('progressbar').firstElementChild).toHaveClass('bg-warning')
    unmount()
    panel('idle', [], { context: { window: 200_000, tokens: 230_000 } })
    expect(screen.getByTestId('info-context-value')).toHaveTextContent('115% · 230k / 200k')
    const bar = within(screen.getByTestId('info-context')).getByRole('progressbar')
    expect(bar).toHaveAttribute('aria-valuenow', '100')
    expect(bar.firstElementChild).toHaveClass('bg-danger')
    expect(bar.firstElementChild).toHaveStyle({ width: '100%' })
  })

  it('shows nothing when the agent does not report it (or reports a zero window)', () => {
    panel('idle', [])
    expect(screen.queryByTestId('info-context')).toBeNull()
    cleanup()
    panel('idle', [], { context: null })
    expect(screen.queryByTestId('info-context')).toBeNull()
    cleanup()
    panel('idle', [], { context: { window: 0, tokens: 10 } })
    expect(screen.queryByTestId('info-context')).toBeNull()
  })

  it('takes the numbers from the latest turn_done in the session view', () => {
    const view = buildView(
      evs([
        { type: 'turn_done', turn_id: 1, reason: 'completed', usage: { duration_ms: 1, context_window: 200_000, context_tokens: 12_000 } },
        { type: 'turn_done', turn_id: 2, reason: 'completed', usage: { duration_ms: 1, context_window: 200_000, context_tokens: 30_000 } },
      ]),
    )
    panel('idle', [], { context: view.context })
    expect(screen.getByTestId('info-context-value')).toHaveTextContent('15% · 30k / 200k')
  })
})
