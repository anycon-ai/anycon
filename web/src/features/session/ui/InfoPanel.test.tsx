// 信息面板：「会话」卡片的各项（名称一行、值一行），「Agent」卡片（主 agent 的状态和 token，子 agent 的状态、token、调用数，
// 在工作的个数，token 变动时的背景动画），上下文占用和账号用量窗口。
import { act, cleanup, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
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

describe('InfoPanel session card', () => {
  it('shows each value in full on its own line under its name, with a button to copy it', () => {
    const workDir = '/Volumes/work/github/anycon/.anycon/worktrees/shiny-otter'
    panel('idle', [], { project: 'anycon', workDir, worktree: { branch: 'anycon/shiny-otter', base: 'origin/main', state: 'active' } as InfoPanelProps['worktree'] })
    expect(screen.getByTestId('info-project')).toHaveTextContent('Projectanycon')
    const dir = screen.getByTestId('info-workdir')
    expect(dir).toHaveTextContent(`Working directory${workDir}`)
    // 不截断，也不靠悬停提示显示全文
    expect(dir.querySelector('.truncate, [title]:not(button)')).toBeNull()
    expect(within(dir).getByTestId('info-workdir-copy')).toHaveAccessibleName('Copy')
    const branch = screen.getByTestId('info-branch')
    expect(branch).toHaveTextContent('Branchanycon/shiny-otter ← origin/main')
    expect(within(branch).getByTestId('info-branch-copy')).toBeInTheDocument()
  })

  it('copies the working directory', async () => {
    const user = userEvent.setup()
    panel('idle', [], { project: 'app', workDir: '/ws/app' })
    await user.click(screen.getByTestId('info-workdir-copy'))
    expect(await navigator.clipboard.readText()).toBe('/ws/app')
  })

  it('has no way to open the working directory unless the page gives it one', () => {
    panel('idle', [], { project: 'app', workDir: '/ws/app' })
    expect(screen.queryByTestId('info-workdir-link')).toBeNull()
  })

  it('has a button that opens the file browser when the page gives it one', async () => {
    let opened = 0
    panel('idle', [], { project: 'app', workDir: '/ws/app', onOpenWorkDir: () => void opened++ })
    const link = within(screen.getByTestId('info-workdir')).getByTestId('info-workdir-link')
    expect(link).toHaveAccessibleName('Browse the files in the working directory')
    expect(link).toHaveAttribute('title', 'Browse the files in the working directory')
    await userEvent.setup().click(link)
    expect(opened).toBe(1)
  })

  it('names a session without a project or directory a temporary workspace', () => {
    panel('idle', [], { onOpenWorkDir: () => {} })
    expect(screen.queryByTestId('info-project')).toBeNull()
    expect(screen.getByTestId('info-workdir')).toHaveTextContent('Temporary workspace')
    expect(screen.getByTestId('info-workdir-link')).toBeInTheDocument()
    expect(screen.queryByTestId('info-workdir-copy')).toBeNull()
  })
})

describe('InfoPanel usage limits', () => {
  const HOUR = 3_600_000

  it('shows one bar per window the agent reported, with the reset time', () => {
    const now = Date.now()
    panel('idle', [], {
      limits: [
        { window_minutes: 300, used_percent: 42, resets_at_ms: now + 2 * HOUR },
        { window_minutes: 10_080, used_percent: 91, resets_at_ms: now + 100 * HOUR },
        { window_minutes: 10_080, used_percent: 130, model: 'opus' },
      ],
    })
    const card = screen.getByTestId('info-limits')
    expect(card).toHaveTextContent('Usage limits')
    expect(card).toHaveTextContent('Claude Code')
    const rows = screen.getAllByTestId('info-limit')
    expect(rows.map((r) => r.dataset.percent)).toEqual(['42', '91', '130'])
    expect(rows[0]).toHaveTextContent('5-hour limit')
    expect(within(rows[0]!).getByTestId('info-limit-value')).toHaveTextContent(/^42% · resets .+/)
    expect(within(rows[0]!).getByRole('progressbar', { name: '5-hour limit' })).toHaveAttribute('aria-valuenow', '42')
    expect(rows[1]).toHaveTextContent('Weekly limit')
    expect(within(rows[1]!).getByRole('progressbar').firstElementChild).toHaveClass('bg-danger')
    // 按模型的窗口带模型名；没有恢复时间只写百分比；超过 100% 画满
    expect(rows[2]).toHaveTextContent('Weekly limit · opus')
    expect(within(rows[2]!).getByTestId('info-limit-value')).toHaveTextContent('130% used')
    expect(within(rows[2]!).getByRole('progressbar')).toHaveAttribute('aria-valuenow', '100')
  })

  it('names other window lengths by their duration', () => {
    panel('idle', [], { limits: [{ window_minutes: 1440, used_percent: 1 }, { window_minutes: 180, used_percent: 2 }, { window_minutes: 90, used_percent: 3 }, { used_percent: 4 }] })
    expect(screen.getAllByTestId('info-limit').map((r) => within(r).getByRole('progressbar').getAttribute('aria-label'))).toEqual(['1-day limit', '3-hour limit', '90-minute limit', 'Usage limit'])
  })

  it('leaves out a window whose reset time has passed, and the card when nothing is left', () => {
    const now = Date.now()
    const { unmount } = panel('idle', [], { limits: [{ window_minutes: 300, used_percent: 80, resets_at_ms: now - HOUR }, { window_minutes: 10_080, used_percent: 5, resets_at_ms: now + HOUR }] })
    expect(screen.getAllByTestId('info-limit')).toHaveLength(1)
    expect(screen.getByTestId('info-limit')).toHaveTextContent('Weekly limit')
    unmount()
    panel('idle', [], { limits: [{ window_minutes: 300, used_percent: 80, resets_at_ms: now - HOUR }] })
    expect(screen.queryByTestId('info-limits')).toBeNull()
    cleanup()
    panel('idle', [])
    expect(screen.queryByTestId('info-limits')).toBeNull()
  })

  it('takes the windows from the latest turn_done that reported them', () => {
    const view = buildView(
      evs([
        { type: 'turn_done', turn_id: 1, reason: 'completed', usage: { duration_ms: 1, limits: [{ window_minutes: 300, used_percent: 10 }] } },
        { type: 'turn_done', turn_id: 2, reason: 'completed', usage: { duration_ms: 1, limits: [{ window_minutes: 300, used_percent: 20 }] } },
        // 被取消的 turn 没报：留着上一次的
        { type: 'turn_done', turn_id: 3, reason: 'cancelled', usage: { duration_ms: 1 } },
      ]),
    )
    expect(view.limits).toEqual([{ window_minutes: 300, used_percent: 20 }])
    expect(buildView(evs([])).limits).toEqual([])
  })
})

describe('InfoPanel agents card activity', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  const base = (over: Partial<InfoPanelProps>): InfoPanelProps => ({
    agentLabel: 'Claude Code',
    status: 'running',
    subagents: [],
    createdAt: 1,
    usage: { turns: 1, inputTokens: 100, outputTokens: 50, cacheReadTokens: 0, cacheWriteTokens: 0, costUsd: null },
    pending: [],
    onFocusPending: () => {},
    archived: false,
    onRename: () => {},
    onArchive: () => {},
    onDelete: () => {},
    onExport: () => {},
    onRestart: () => {},
    ...over,
  })
  const card = (): HTMLElement => screen.getByTestId('info-agents')
  const sweeps = (): number => card().querySelectorAll('.token-sweep').length
  const tick = (ms: number): void => void act(() => void vi.advanceTimersByTime(ms))

  it('sweeps the background when the token numbers change, not on the first render', () => {
    const { rerender } = render(<InfoPanel {...base({ status: 'idle' })} />)
    tick(2000)
    expect(sweeps()).toBe(0)
    rerender(<InfoPanel {...base({ status: 'idle', usage: { turns: 2, inputTokens: 300, outputTokens: 90, cacheReadTokens: 0, cacheWriteTokens: 0, costUsd: null } })} />)
    tick(0)
    expect(sweeps()).toBe(1)
    expect(card().dataset.pulse).toBe('1')
    // 别的 props 变了、token 没变：不动
    rerender(<InfoPanel {...base({ status: 'idle', agentLabel: 'Codex', usage: { turns: 2, inputTokens: 300, outputTokens: 90, cacheReadTokens: 0, cacheWriteTokens: 0, costUsd: null } })} />)
    tick(2000)
    expect(card().dataset.pulse).toBe('1')
  })

  it('sweeps while a turn is running and events keep arriving, one after another instead of all at once', () => {
    const { rerender } = render(<InfoPanel {...base({ activitySeq: 10 })} />)
    rerender(<InfoPanel {...base({ activitySeq: 11 })} />)
    tick(0)
    expect(card().dataset.pulse).toBe('1')
    // 一道光还没扫完又来了很多事件：合并成扫完之后的一次
    for (let seq = 12; seq < 40; seq++) rerender(<InfoPanel {...base({ activitySeq: seq })} />)
    tick(600)
    expect(card().dataset.pulse).toBe('1')
    tick(600)
    expect(card().dataset.pulse).toBe('2')
    // 没有新事件就停下
    tick(5000)
    expect(card().dataset.pulse).toBe('2')
    expect(sweeps()).toBe(1)
  })

  it('does not sweep for new events when no turn is running', () => {
    const { rerender } = render(<InfoPanel {...base({ status: 'idle', activitySeq: 10 })} />)
    rerender(<InfoPanel {...base({ status: 'idle', activitySeq: 11 })} />)
    tick(2000)
    expect(sweeps()).toBe(0)
  })
})
