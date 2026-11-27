// Bot 页（运行状态页）：状态优先级、暂停 / 恢复 / 现在看一眼 / 试跑、没配置的卡片、今天的数字、健康提醒、现在、趋势、运行记录，
// 以及没有 `twin_stats` 时的降级。配置（范围、预算、判断后端、Bot 连接）的测试在 TwinTab.test.tsx。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo, TwinGetResult } from '@/protocol'
import { nodeEntry, mockAgent, project } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTwinDaemon, twinDay, twinQuestion, twinRun, twinValue } from '@/test/fakeTwinDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useTwin } from '@/features/twin/store'
import { syncTwin } from '@/features/twin/service'
import { handleNotification } from '@/features/sync/notifications'
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/twin' }))
import { TwinPage } from './TwinPage'
const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
let rpc: FakeRpc
let daemon: FakeTwinDaemon
const info = (twin: boolean, stats = false): NodeInfo =>
  ({ capabilities: { twin, todo: true, ...(stats ? { twin_stats: true } : {}) }, judge: { triage: 'triage', backends: [{ name: 'triage' }], daily_calls: 20 } }) as unknown as NodeInfo
const midnight = new Date().setHours(0, 0, 0, 0)
function setNode(stats: boolean): void {
  useNodes.setState({
    byId: {
      [A]: nodeEntry({
        id: A,
        name: 'alpha',
        info: info(true, stats),
        projects: [project({ name: 'web' }), project({ name: 'api' })],
        agents: [mockAgent({ modes: [{ id: 'ask', label: 'Ask', risk: 'normal' }, { id: 'auto', label: 'Auto', risk: 'normal', unattended: true }] })],
      }),
    },
    order: [A],
    loaded: true,
  })
}
beforeEach(() => {
  rpc = new FakeRpc()
  daemon = new FakeTwinDaemon(rpc)
  setRpc(rpc)
  useTwin.getState().reset()
  usePrefs.setState({ sidebarNodes: null, sidebarSpace: null, twinCollapsed: [] })
  setNode(false)
})
async function load(props: { node?: string; run?: string } = {}) {
  render(<TwinPage {...props} />)
  await waitFor(() => expect(screen.getByTestId('twin-state')).not.toHaveTextContent('Loading…'))
}
const calls = (method: string) => rpc.calls.filter((c) => c.method === method)
const set = (over: Partial<TwinGetResult>) => daemon.values.set(A, twinValue(over))
const paused = (over: Partial<TwinGetResult> = {}) => set({ state: 'paused', scope: { paused: true, projects: ['web'], max_sessions: 2, daily_dispatches: 10 }, ...over })

describe('Bot page status head', () => {
  it.each(['idle', 'running', 'paused', 'budget', 'not_configured', 'disabled'] as const)('renders daemon state %s', async (state) => {
    set({ state, enabled: state !== 'disabled', configured: state !== 'not_configured' })
    await load()
    expect(screen.getByTestId('twin-state')).toHaveTextContent(
      { idle: 'On', running: 'On · judging', paused: 'Paused', budget: 'Budget used up', not_configured: 'Not configured', disabled: 'Bot is switched off on this machine' }[state],
    )
  })
  it('says not configured even when the bot is also paused', async () => {
    set({ state: 'paused', configured: false, scope: { paused: true, projects: [], max_sessions: 2, daily_dispatches: 10 } })
    await load()
    expect(screen.getByTestId('twin-state')).toHaveTextContent('Not configured')
  })
  it('says the machine switch wins over everything else', async () => {
    set({ enabled: false, configured: false, state: 'disabled' })
    await load()
    expect(screen.getByTestId('twin-state')).toHaveTextContent('switched off on this machine')
    expect(screen.queryByTestId('twin-setup')).not.toBeInTheDocument()
  })
  it('offers retry when loading failed', async () => {
    rpc.handlers['twin.get'] = () => {
      throw new Error('boom')
    }
    render(<TwinPage />)
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not load Bot.')
    expect(screen.getByTestId('twin-state')).toHaveTextContent('Could not load Bot.')
    rpc.handlers['twin.get'] = () => twinValue()
    await userEvent.click(screen.getByRole('button', { name: 'Retry' }))
    await waitFor(() => expect(screen.getByTestId('twin-state')).toHaveTextContent('On'))
  })
  it('shows offline and hides actions; unsupported nodes disappear', async () => {
    useNodes.getState().patch(A, { conn: 'peer_offline' })
    render(<TwinPage />)
    expect(screen.getByText('Node offline')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Pause' })).not.toBeInTheDocument()
    expect(calls('twin.get')).toHaveLength(0)
    act(() => useNodes.getState().patch(A, { info: info(false) }))
    expect(screen.queryByTestId('twin-card')).not.toBeInTheDocument()
  })
  it('a node whose daemon is too old for Bot says so, without the Inbox filter note that belongs to other pages', async () => {
    useNodes.setState((s) => ({ byId: { ...s.byId, [B]: nodeEntry({ id: B, name: 'old-node', info: info(false) }) }, order: [A, B] }))
    render(<TwinPage node={B} />)
    expect(screen.getByText('old-node does not support Bot yet. Update its daemon to use Bot there.')).toBeInTheDocument()
    expect(screen.queryByText(/node filter/)).not.toBeInTheDocument()
    expect(screen.queryByTestId('twin-card')).not.toBeInTheDocument()
  })
  it('no longer carries the scope form or the connection section', async () => {
    await load()
    expect(screen.queryByRole('button', { name: 'Save scope' })).not.toBeInTheDocument()
    expect(screen.queryByTestId('twin-links')).not.toBeInTheDocument()
    expect(calls('twin.links')).toHaveLength(0)
    expect(screen.queryByText(/node settings → Limits/)).not.toBeInTheDocument()
  })
  it('only includes selected nodes', async () => {
    useNodes.setState((s) => ({ byId: { ...s.byId, [B]: nodeEntry({ id: B, name: 'beta', info: info(true) }) }, order: [A, B] }))
    usePrefs.setState({ sidebarNodes: [B] })
    await syncTwin(B)
    render(<TwinPage />)
    expect(screen.queryByText('alpha')).not.toBeInTheDocument()
    expect(screen.getByText('beta')).toBeInTheDocument()
  })
})

describe('Bot page card collapse', () => {
  it('collapses one card to its head, keeps the actions, and remembers it', async () => {
    useNodes.setState((s) => ({ byId: { ...s.byId, [B]: nodeEntry({ id: B, name: 'beta', info: info(true) }) }, order: [A, B] }))
    render(<TwinPage />)
    await waitFor(() => expect(screen.getAllByTestId('twin-config-summary')).toHaveLength(2))
    const [alpha, beta] = screen.getAllByTestId('twin-card')
    const toggle = within(alpha!).getByRole('button', { name: 'alpha' })
    expect(toggle).toHaveAttribute('aria-expanded', 'true')
    await userEvent.click(toggle)
    expect(toggle).toHaveAttribute('aria-expanded', 'false')
    expect(within(alpha!).queryByTestId('twin-config-summary')).not.toBeInTheDocument()
    expect(within(alpha!).getByRole('button', { name: 'Pause' })).toBeInTheDocument()
    expect(within(beta!).getByTestId('twin-config-summary')).toBeInTheDocument()
    expect(usePrefs.getState().twinCollapsed).toEqual([A])
    await userEvent.click(toggle)
    expect(within(alpha!).getByTestId('twin-config-summary')).toBeInTheDocument()
    expect(usePrefs.getState().twinCollapsed).toEqual([])
  })
  it('a run link opens a collapsed card', async () => {
    daemon.runs = [twinRun({ id: 'r1' })]
    usePrefs.setState({ twinCollapsed: [A] })
    render(<TwinPage node={A} run="r1" />)
    expect(await screen.findByTestId('twin-run')).toBeInTheDocument()
    expect(usePrefs.getState().twinCollapsed).toEqual([])
  })
})

describe('Bot page actions', () => {
  it('pauses one node, or all nodes in the selected space', async () => {
    useNodes.setState((s) => ({ byId: { ...s.byId, [B]: nodeEntry({ id: B, name: 'beta', info: info(true) }) }, order: [A, B] }))
    render(<TwinPage />)
    await waitFor(() => expect(screen.getAllByRole('button', { name: 'Pause' })).toHaveLength(2))
    const alpha = screen.getAllByTestId('twin-card')[0]!
    await userEvent.click(within(alpha).getByRole('button', { name: 'Pause' }))
    await waitFor(() => expect(daemon.values.get(A)?.scope.paused).toBe(true))
    await userEvent.click(screen.getByRole('button', { name: 'Pause all' }))
    await waitFor(() => expect(daemon.values.get(B)?.scope.paused).toBe(true))
    expect(calls('twin.set').every((c) => (c.params as { paused: boolean }).paused)).toBe(true)
  })
  it('shows Resume instead of Pause and Look now when paused', async () => {
    paused()
    await load()
    expect(screen.getByRole('button', { name: 'Resume' })).toBeEnabled()
    expect(screen.queryByRole('button', { name: 'Pause' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Look now' })).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Try a dry run' })).toBeEnabled()
  })
  it('resume runs a dry run first, shows the plan, and only "Do it" unpauses and wakes', async () => {
    paused()
    daemon.dry = twinRun({ dry_run: true, decisions: [{ todo_id: 't1', action: 'dispatch', reason: 'Need more slots', rules: [], outcome: 'skipped', skip_reason: 'no_capacity' }] })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    expect(await within(dialog).findByText('Need more slots')).toBeInTheDocument()
    expect(within(dialog).getByText('Skipped: No available session slots')).toBeInTheDocument()
    expect(calls('twin.set')).toHaveLength(0)
    expect(calls('twin.wake')).toHaveLength(0)
    await userEvent.click(within(dialog).getByRole('button', { name: 'Do it this way' }))
    await waitFor(() => expect(calls('twin.wake')).toHaveLength(1))
    expect(calls('twin.set')[0]?.params).toEqual({ paused: false })
    await waitFor(() => expect(screen.queryByTestId('twin-plan-dialog')).not.toBeInTheDocument())
  })
  it('cancel leaves the bot paused', async () => {
    paused()
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    await within(dialog).findByTestId('twin-decision')
    await userEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }))
    expect(calls('twin.set')).toHaveLength(0)
    expect(calls('twin.wake')).toHaveLength(0)
  })
  it.each(['failed', 'budget', 'cancelled'] as const)('cannot resume from a dry run that ended as %s', async (result) => {
    paused()
    daemon.dry = twinRun({ dry_run: true, result, ...(result === 'failed' ? { error: { code: 'internal' } } : {}) })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    expect(await within(dialog).findByText(/Bot cannot resume yet/)).toBeInTheDocument()
    expect(within(dialog).getByRole('button', { name: 'Do it this way' })).toBeDisabled()
    expect(within(dialog).getByRole('button', { name: 'Resume without a dry run' })).toBeEnabled()
    expect(calls('twin.wake')).toHaveLength(0)
  })
  it('explains a budget-exhausted dry run and resumes without a dry run', async () => {
    paused()
    daemon.dry = twinRun({ dry_run: true, result: 'budget' })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    expect(await within(dialog).findByText(/judgment-call budget is used up/)).toBeInTheDocument()
    expect(within(dialog).getByRole('button', { name: 'Do it this way' })).toBeDisabled()
    await userEvent.click(within(dialog).getByRole('button', { name: 'Resume without a dry run' }))
    await waitFor(() => expect(calls('twin.wake')).toHaveLength(1))
    expect(calls('twin.set')[0]?.params).toEqual({ paused: false })
    await waitFor(() => expect(screen.queryByTestId('twin-plan-dialog')).not.toBeInTheDocument())
  })
  it('offers resuming without a dry run when the dry-run request itself fails', async () => {
    paused()
    rpc.handlers['twin.dry_run'] = () => {
      throw rpcError('internal')
    }
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    expect(await within(dialog).findByRole('button', { name: 'Resume without a dry run' })).toBeEnabled()
  })
  it('does not offer resuming without a dry run when the plan is usable', async () => {
    paused()
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    await waitFor(() => expect(within(dialog).getByRole('button', { name: 'Do it this way' })).toBeEnabled())
    expect(within(dialog).queryByRole('button', { name: 'Resume without a dry run' })).not.toBeInTheDocument()
  })
  it('shows a failed set inside the dialog, keeps it open and usable (Do it and resume without a dry run)', async () => {
    paused()
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    const doIt = await within(dialog).findByRole('button', { name: 'Do it this way' })
    await waitFor(() => expect(doIt).toBeEnabled())
    daemon.failSet = true
    await userEvent.click(doIt)
    expect(await within(dialog).findByRole('alert', { name: '' })).toBeInTheDocument()
    expect(within(dialog).getByText(/max_sessions|invalid/i)).toBeInTheDocument()
    expect(screen.getByTestId('twin-plan-dialog')).toBeInTheDocument()
    await waitFor(() => expect(doIt).toBeEnabled())
    expect(calls('twin.wake')).toHaveLength(0)
  })
  it('a failed set through resume without a dry run shows the error inside the dialog', async () => {
    paused()
    daemon.dry = twinRun({ dry_run: true, result: 'budget' })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    daemon.failSet = true
    await userEvent.click(await within(dialog).findByRole('button', { name: 'Resume without a dry run' }))
    await waitFor(() => expect(within(dialog).getAllByRole('alert').length).toBeGreaterThan(1))
    expect(within(dialog).getByRole('button', { name: 'Resume without a dry run' })).toBeEnabled()
  })
  it('closes the dialog once the set succeeds and reports a failed wake on the card', async () => {
    paused()
    rpc.handlers['twin.wake'] = () => {
      throw rpcError('internal')
    }
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    const doIt = await within(dialog).findByRole('button', { name: 'Do it this way' })
    await waitFor(() => expect(doIt).toBeEnabled())
    await userEvent.click(doIt)
    await waitFor(() => expect(screen.queryByTestId('twin-plan-dialog')).not.toBeInTheDocument())
    expect(await screen.findByText(/Bot is resumed, but waking it failed/)).toBeInTheDocument()
    expect(daemon.values.get(A)?.scope.paused).toBe(false)
    expect(screen.queryByRole('button', { name: 'Resume' })).not.toBeInTheDocument()
  })
  it('an idle dry run can be resumed', async () => {
    paused()
    daemon.dry = twinRun({ dry_run: true, result: 'idle', decisions: [], items: [] })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    await waitFor(() => expect(within(dialog).getByRole('button', { name: 'Do it this way' })).toBeEnabled())
  })
  it('Look now wakes once and says the wake is coalesced', async () => {
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Look now' }))
    expect(await screen.findByRole('status')).toHaveTextContent('within about a minute')
    expect(calls('twin.wake')).toHaveLength(1)
  })
  it('clears the "within a minute" note when a new run arrives or the bot is paused', async () => {
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Look now' }))
    expect(await screen.findByRole('status')).toHaveTextContent('within about a minute')
    daemon.runs = [twinRun({ id: 'run-new', started_ms: 9000 })]
    await act(async () => {
      await syncTwin(A)
    })
    await waitFor(() => expect(screen.queryByText(/within about a minute/)).not.toBeInTheDocument())
    await userEvent.click(screen.getByRole('button', { name: 'Look now' }))
    expect(await screen.findByText(/within about a minute/)).toBeInTheDocument()
    await userEvent.click(screen.getByRole('button', { name: 'Pause' }))
    await waitFor(() => expect(screen.queryByText(/within about a minute/)).not.toBeInTheDocument())
  })
  it('hides Look now and Dry run when the machine switch is off', async () => {
    set({ enabled: false, state: 'disabled' })
    await load()
    expect(screen.queryByRole('button', { name: 'Look now' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Try a dry run' })).not.toBeInTheDocument()
  })
  it('a plain dry run lists the plan but has no "Do it" and executes nothing', async () => {
    daemon.dry = twinRun({ dry_run: true })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Try a dry run' }))
    const dialog = await screen.findByTestId('twin-plan-dialog')
    expect(await within(dialog).findByText('Ready to build')).toBeInTheDocument()
    expect(within(dialog).queryByRole('button', { name: 'Do it this way' })).not.toBeInTheDocument()
    expect(calls('twin.set')).toHaveLength(0)
    expect(calls('twin.wake')).toHaveLength(0)
  })
  it('shows the complete dry-run plan as agent-written plain text', async () => {
    daemon.dry = twinRun({
      dry_run: true,
      decisions: [
        { todo_id: 't1', action: 'dispatch', reason: 'Ready', rules: [], outcome: 'executed', note: '<script>send()</script>\nKeep the API stable.' },
        { todo_id: 't2', action: 'ask_user', reason: 'Need a preference', rules: [], outcome: 'executed', question: '<b>Which platform?</b>', options: ['<img src=x onerror=send()>', '**Web**'] },
      ],
    })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Try a dry run' }))
    expect(await screen.findByText('Bot’s supplementary instructions to the agent')).toBeInTheDocument()
    expect(screen.getByText('<script>send()</script> Keep the API stable.')).toBeInTheDocument()
    expect(screen.getByText('<b>Which platform?</b>')).toBeInTheDocument()
    expect(screen.getByText('<img src=x onerror=send()>')).toBeInTheDocument()
    for (const row of screen.getAllByTestId('twin-decision')) expect(row.querySelector('script, img, b, strong')).toBeNull()
    expect(screen.getAllByText('Written by an agent')).toHaveLength(1)
  })
  it('each referenced rule links to its own node and rule', async () => {
    daemon.dry = twinRun({ dry_run: true, decisions: [{ todo_id: 't1', action: 'dispatch', reason: 'Ready', rules: ['rule1', 'rule2'], outcome: 'executed' }] })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Try a dry run' }))
    expect(await screen.findByRole('link', { name: 'Rules: rule1' })).toHaveAttribute('href', '/rules?node=aaaaaaaa&rule=rule1')
    expect(screen.getByRole('link', { name: 'Rules: rule2' })).toHaveAttribute('href', '/rules?node=aaaaaaaa&rule=rule2')
  })
  it('shows which node a dispatch went to', async () => {
    useNodes.setState((s) => ({ byId: { ...s.byId, [B]: nodeEntry({ id: B, name: 'beta', info: info(true) }) }, order: [A, B] }))
    daemon.dry = twinRun({
      dry_run: true,
      decisions: [
        { todo_id: 't1', action: 'dispatch', reason: 'Build it there', rules: [], outcome: 'executed', node: B, project: 'api' },
        { todo_id: 't4', action: 'dispatch', reason: 'Here', rules: [], outcome: 'executed' },
      ],
    })
    render(<TwinPage node={A} />)
    await userEvent.click(await screen.findByRole('button', { name: 'Try a dry run' }))
    await screen.findByText('Build it there')
    const rows = screen.getAllByTestId('twin-decision')
    expect(rows[0]).toHaveTextContent('Sent to node: beta')
    expect(within(rows[1]!).queryByTestId('twin-decision-node')).not.toBeInTheDocument()
  })
})

describe('Bot page when not configured', () => {
  it('shows the head, what is missing and one set-up link, and nothing else', async () => {
    set({ configured: false, state: 'not_configured', scope: { paused: false, projects: [], max_sessions: 2, daily_dispatches: 10 } })
    setNode(true)
    // 没有可用的分拣后端
    useNodes.getState().patch(A, { info: { capabilities: { twin: true, twin_stats: true }, judge: { triage: 'triage', backends: [] } } as unknown as NodeInfo })
    await load()
    expect(screen.getByText('No projects selected yet.')).toBeInTheDocument()
    expect(screen.queryByText(/below/)).not.toBeInTheDocument()
    expect(screen.getByText(/No usable triage backend/)).toBeInTheDocument()
    const link = screen.getByRole('link', { name: 'Set up' })
    expect(link).toHaveAttribute('href', '/n/aaaaaaaa/settings?tab=twin')
    // 卡片里只有这一个链接（顶栏里的 Bot / 规则标签不算）
    expect(within(screen.getByTestId('twin-card')).getAllByRole('link')).toHaveLength(1)
    expect(screen.queryByTestId('twin-today')).not.toBeInTheDocument()
    expect(screen.queryByTestId('twin-now')).not.toBeInTheDocument()
    expect(screen.queryByTestId('twin-trend')).not.toBeInTheDocument()
    expect(screen.queryByTestId('twin-config-summary')).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Pause' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Try a dry run' })).not.toBeInTheDocument()
    expect(screen.queryByText('Run history')).not.toBeInTheDocument()
  })
  it('still lists earlier runs when there are some', async () => {
    set({ configured: false, state: 'not_configured' })
    daemon.runs = [twinRun()]
    await load()
    expect(await screen.findByText('Run history')).toBeInTheDocument()
    expect(screen.getAllByTestId('twin-run')).toHaveLength(1)
  })
})

describe('Bot page overview', () => {
  it('summarises the configuration on one line with a settings link', async () => {
    set({ scope: { paused: false, projects: ['web', 'api'], agent: 'mock', mode: 'auto', max_sessions: 3, daily_dispatches: 12 } })
    await load()
    const line = await screen.findByTestId('twin-config-summary')
    expect(line).toHaveTextContent('2 projects · Mock · Auto · does not push · up to 3 sessions · 12 dispatches a day · Settings')
    expect(within(line).getByRole('link', { name: 'Settings' })).toHaveAttribute('href', '/n/aaaaaaaa/settings?tab=twin')
  })
  it('falls back to the node default for agent and mode', async () => {
    await load()
    expect(await screen.findByTestId('twin-config-summary')).toHaveTextContent('1 project · Node default · Node default')
  })
  it('shows the raw id for an agent or mode the node does not list', async () => {
    set({ scope: { paused: false, projects: ['web'], agent: 'gone', mode: 'odd', max_sessions: 2, daily_dispatches: 10 } })
    await load()
    expect(await screen.findByTestId('twin-config-summary')).toHaveTextContent('1 project · gone · odd')
  })

  describe('with twin_stats', () => {
    beforeEach(() => {
      setNode(true)
      set({
        today: { dispatches: 4, judge_calls: 7 },
        last_run: { id: 'r9', started_ms: Date.now() - 60_000, finished_ms: Date.now() - 50_000, result: 'done', triggers: ['todo'] },
        next_wake_ms: Date.now() + 30_000,
        queued: ['q1'],
        deferred: ['d1', 'd2'],
        working: ['w1'],
      })
      daemon.statsDays = [
        twinDay({
          date: '2026-01-02',
          runs: { done: 3, idle: 5, failed: 1, cancelled: 1, budget: 1 },
          usage: { cost_usd: 0.42 },
        }),
      ]
    })
    it('takes wakes and cost from twin.stats, and last / next wake from twin.get', async () => {
      await load()
      const wakes = await screen.findByTestId('twin-tile-wakes')
      expect(wakes).toHaveTextContent('11')
      expect(wakes).toHaveTextContent('acted 3')
      expect(wakes).toHaveTextContent('failed 1')
      expect(wakes).toHaveTextContent('idle 5')
      // 取消和预算用完不算失败，单独写
      expect(wakes).toHaveTextContent('stopped early 2')
      expect(screen.getByTestId('twin-tile-dispatches')).toHaveTextContent('4 / 10')
      expect(screen.getByTestId('twin-tile-dispatches')).toHaveTextContent('6 left today')
      expect(screen.getByTestId('twin-tile-judges')).toHaveTextContent('7 / 20')
      expect(screen.getByTestId('twin-tile-judges')).toHaveTextContent('13 left today')
      expect(screen.getByTestId('twin-tile-cost')).toHaveTextContent('$0.42')
      expect(screen.getByTestId('twin-tile-last')).toHaveTextContent('To-do changed · Completed')
      expect(screen.getByTestId('twin-tile-next')).not.toHaveTextContent('When something changes')
      expect(calls('twin.stats')[0]?.params).toEqual({ days: 7 })
    })
    it('shows tokens when there is no cost, and hides the cost tile without usage', async () => {
      daemon.statsDays = [twinDay({ usage: { input_tokens: 12000, output_tokens: 300 } })]
      await load()
      const cost = await screen.findByTestId('twin-tile-cost')
      expect(cost).toHaveTextContent('12k in')
      expect(cost).toHaveTextContent('300 tokens out')
    })
    it('hides the cost tile when there is no usage', async () => {
      daemon.statsDays = [twinDay()]
      await load()
      await screen.findByTestId('twin-tile-wakes')
      expect(screen.queryByTestId('twin-tile-cost')).not.toBeInTheDocument()
    })
    it('says the daily limit is used up when dispatches reach it', async () => {
      set({ today: { dispatches: 10, judge_calls: 3 } })
      await load()
      const tile = await screen.findByTestId('twin-tile-dispatches')
      expect(tile).toHaveTextContent('10 / 10')
      expect(tile).toHaveTextContent('Today’s limit is used up')
      expect(tile.querySelector('[data-tone]')).toHaveAttribute('data-tone', 'danger')
      expect(screen.getByTestId('twin-tile-judges').querySelector('[data-tone]')).toHaveAttribute('data-tone', 'accent')
    })
    it('says "never" without a last wake, and when the next wake depends on changes', async () => {
      set({ last_run: null, next_wake_ms: null })
      await load()
      expect(await screen.findByTestId('twin-tile-last')).toHaveTextContent('Never')
      expect(screen.getByTestId('twin-tile-next')).toHaveTextContent('When something changes')
    })
    it('does not promise a next wake while paused', async () => {
      paused({ next_wake_ms: null })
      await load()
      expect(await screen.findByTestId('twin-tile-next')).not.toHaveTextContent('When something changes')
    })
    it('lists Working, Queued and Not looked at last time, and collapses the empty ones', async () => {
      daemon.questions = []
      await load()
      expect(await screen.findByRole('heading', { name: 'Working on (1)' })).toBeInTheDocument()
      expect(screen.getByRole('heading', { name: 'Queued (1)' })).toBeInTheDocument()
      expect(screen.getByRole('heading', { name: 'Not looked at last time (2)' })).toBeInTheDocument()
      expect(screen.getByText(/ran out of input space/)).toBeInTheDocument()
      expect(screen.queryByRole('heading', { name: /Waiting for you/ })).not.toBeInTheDocument()
      expect(screen.getByRole('link', { name: 'd1' })).toHaveAttribute('href', '/todos/aaaaaaaa/d1')
    })
    it('says so in one line when nothing is going on', async () => {
      set({ working: [], queued: [], deferred: [] })
      await load()
      expect(await screen.findByText('Nothing in progress, waiting for you or queued.')).toBeInTheDocument()
      expect(screen.queryByRole('heading', { name: /Queued/ })).not.toBeInTheDocument()
    })

    describe('trend', () => {
      beforeEach(() => {
        daemon.statsDays = [
          twinDay({
            date: '2026-01-01',
            runs: { done: 1 },
            decisions: { dispatch: 2, ask_user: 1 },
            handed_back: 1,
            taken_over: 1,
            accepted: 1,
            turnaround_ms: 120_000,
            usage: { cost_usd: 0.1 },
          }),
          twinDay({ date: '2026-01-02', runs: { done: 2, idle: 3, failed: 1 }, decisions: { dispatch: 2 }, accepted: 1, turnaround_ms: 240_000, dropped: 2, usage: { cost_usd: 0.2 } }),
        ]
      })
      it('shows the range summary with plain counts and a day table, newest first', async () => {
        await load()
        const summary = await screen.findByTestId('twin-trend-summary')
        expect(within(summary).getByTestId('twin-trend-wakes')).toHaveTextContent('7')
        expect(within(summary).getByTestId('twin-trend-dispatches')).toHaveTextContent('4')
        expect(within(summary).getByTestId('twin-trend-questions')).toHaveTextContent('1')
        expect(within(summary).getByTestId('twin-trend-handed-back')).toHaveTextContent(/^Handed back to you1$/)
        expect(within(summary).getByTestId('twin-trend-taken-over')).toHaveTextContent(/^You took over1$/)
        expect(within(summary).getByTestId('twin-trend-accepted')).toHaveTextContent('2')
        expect(within(summary).getByTestId('twin-trend-dropped')).toHaveTextContent('2')
        expect(within(summary).getByTestId('twin-trend-turnaround')).toHaveTextContent('3m 0s')
        expect(within(summary).getByTestId('twin-trend-cost')).toHaveTextContent('$0.30')
        const table = screen.getByRole('table', { name: 'Bot activity per day, newest first' })
        expect(within(table).getAllByRole('columnheader').map((h) => h.textContent)).toContain('Date')
        const rows = within(table).getAllByTestId('twin-trend-row')
        expect(rows).toHaveLength(7)
        expect(rows[0]).toHaveTextContent('2026-01-02')
        expect(rows[0]).toHaveTextContent('6 (2 acted / 3 idle / 1 failed)')
        expect(rows[1]).toHaveTextContent('2026-01-01')
      })
      it('draws one column per day and reads a day with the arrow keys', async () => {
        await load()
        await screen.findByTestId('twin-trend-summary')
        const chart = screen.getByTestId('twin-trend-chart')
        expect(within(chart).getAllByTestId('twin-chart-day')).toHaveLength(7)
        // 每天的唤醒最多 6 次 → 纵轴上限 10（1、2、5 × 10 的幂里不小于一半的那个再乘 2）
        expect(chart).toHaveTextContent('10')
        const readout = within(chart).getByTestId('twin-chart-readout')
        // 没选中哪天时读的是今天（最后一天）
        expect(readout).toHaveTextContent('Today · 2026-01-02')
        expect(readout).toHaveTextContent('6 wakes')
        const plot = within(chart).getByRole('group', { name: /Wakes per day/ })
        act(() => plot.focus())
        await userEvent.keyboard('{ArrowLeft}')
        expect(readout).toHaveTextContent('2026-01-01')
        expect(readout).not.toHaveTextContent('Today')
        expect(readout).toHaveTextContent('1 wake')
        expect(readout).toHaveTextContent('2 dispatches')
        act(() => plot.blur())
        expect(readout).toHaveTextContent('Today · 2026-01-02')
      })
      it('says so on the chart when there were no wakes in the range', async () => {
        daemon.statsDays = [twinDay({ runs: {} })]
        await load()
        expect(await screen.findByText('No wakes in this range')).toBeInTheDocument()
      })
      it('keeps the day table behind a "Day by day" disclosure', async () => {
        await load()
        await screen.findByTestId('twin-trend-summary')
        const details = screen.getByTestId('twin-trend-table').closest('details')!
        expect(details).not.toHaveAttribute('open')
        await userEvent.click(within(details).getByText('Day by day'))
        expect(details).toHaveAttribute('open')
      })
      it('shows a dash for turnaround when nothing was dispatched or accepted', async () => {
        daemon.statsDays = [twinDay({ runs: { idle: 2 } })]
        await load()
        const summary = await screen.findByTestId('twin-trend-summary')
        expect(within(summary).getByTestId('twin-trend-taken-over')).toHaveTextContent(/^You took over0$/)
        expect(within(summary).getByTestId('twin-trend-turnaround')).toHaveTextContent('—')
      })
      it('switches to 30 days on demand and refreshes when twin.changed arrives', async () => {
        await load()
        await screen.findByTestId('twin-trend-summary')
        await userEvent.click(screen.getByRole('radio', { name: '30 days' }))
        await waitFor(() => expect(calls('twin.stats').some((c) => (c.params as { days: number }).days === 30)).toBe(true))
        await waitFor(() => expect(screen.getAllByTestId('twin-trend-row')).toHaveLength(30))
        const before = calls('twin.stats').length
        act(() => handleNotification(A, 'twin.changed', { state: 'idle' }))
        await waitFor(() => expect(calls('twin.stats').length).toBe(before + 1))
        expect((calls('twin.stats').at(-1)?.params as { days: number }).days).toBe(30)
      })
      it('shows an error with Retry when statistics fail, keeps the page working, and recovers', async () => {
        daemon.failStats = true
        await load()
        const trend = await screen.findByTestId('twin-trend')
        await within(trend).findByText('Could not load statistics.')
        expect(trend).not.toHaveTextContent('Loading statistics…')
        expect(screen.getByTestId('twin-tile-dispatches')).toBeInTheDocument()
        expect(screen.getByTestId('twin-state')).not.toHaveTextContent('Could not load')
        daemon.failStats = false
        await userEvent.click(within(trend).getByRole('button', { name: 'Retry' }))
        await screen.findByTestId('twin-trend-summary')
        expect(within(trend).queryByText('Could not load statistics.')).not.toBeInTheDocument()
      })
      it('ends in the error state when a range switch fails', async () => {
        await load()
        await screen.findByTestId('twin-trend-summary')
        daemon.failStats = true
        await userEvent.click(screen.getByRole('radio', { name: '30 days' }))
        await screen.findByText('Could not load statistics.')
        expect(screen.queryByText('Loading statistics…')).not.toBeInTheDocument()
      })
      it('does not fetch statistics from a sync nobody shows, and the page does', async () => {
        useNodes.getState().patch(A, { conn: 'ready' })
        await act(async () => {
          await syncTwin(A)
        })
        expect(calls('twin.get').length).toBeGreaterThan(0)
        expect(calls('twin.stats')).toHaveLength(0)
        await load()
        await waitFor(() => expect(calls('twin.stats').length).toBeGreaterThan(0))
      })
    })
  })

  describe('without twin_stats', () => {
    it('derives today from the loaded runs and hides cost, next wake, queued, deferred and the trend', async () => {
      set({ queued: ['q1'], deferred: ['d1'], next_wake_ms: Date.now() + 1000 })
      daemon.runs = [
        twinRun({ id: 'r4', started_ms: midnight + 4000, result: 'done', triggers: ['manual'], usage: { cost_usd: 1 } }),
        twinRun({ id: 'r3', started_ms: midnight + 3000, result: 'idle', decisions: [], items: [] }),
        twinRun({ id: 'r2', started_ms: midnight + 2000, result: 'failed', error: { code: 'timeout' } }),
        twinRun({ id: 'r1', started_ms: midnight - 5000, result: 'done' }),
        twinRun({ id: 'dry', started_ms: midnight + 5000, dry_run: true }),
      ]
      await load()
      const wakes = await screen.findByTestId('twin-tile-wakes')
      expect(wakes).toHaveTextContent('3')
      expect(wakes).toHaveTextContent('acted 1')
      expect(wakes).toHaveTextContent('failed 1')
      expect(wakes).toHaveTextContent('idle 1')
      expect(wakes).not.toHaveTextContent('stopped early')
      expect(screen.getByTestId('twin-tile-last')).toHaveTextContent('Manual wake · Completed')
      expect(screen.queryByTestId('twin-tile-cost')).not.toBeInTheDocument()
      expect(screen.queryByTestId('twin-tile-next')).not.toBeInTheDocument()
      expect(screen.queryByTestId('twin-trend')).not.toBeInTheDocument()
      expect(screen.queryByRole('heading', { name: /Queued/ })).not.toBeInTheDocument()
      expect(screen.queryByRole('heading', { name: /Not looked at/ })).not.toBeInTheDocument()
      expect(calls('twin.stats')).toHaveLength(0)
    })
    it('says never when there are no runs', async () => {
      await load()
      expect(await screen.findByTestId('twin-tile-last')).toHaveTextContent('Never')
    })
    it('shows just the number of judgment calls when the daily limit is unknown', async () => {
      useNodes.getState().patch(A, { info: { capabilities: { twin: true, todo: true } } as unknown as NodeInfo })
      await load()
      expect((await screen.findByTestId('twin-tile-judges')).textContent).toContain('3')
      expect(screen.getByTestId('twin-tile-judges')).not.toHaveTextContent('/')
    })
  })

  it('lists who is waiting for you with how long, linking to the inbox entry', async () => {
    set({ waiting: ['t1', 't2'] })
    daemon.questions = [twinQuestion({ created_ms: Date.now() - 125_000 })]
    await load()
    const link = await screen.findByRole('link', { name: 't1' })
    expect(link).toHaveAttribute('href', '/inbox?node=aaaaaaaa&todo=t1')
    expect(link.parentElement).toHaveTextContent(/waiting 2m/)
    expect(screen.getByRole('link', { name: 't2' })).toHaveAttribute('href', '/todos/aaaaaaaa/t2')
  })
})

describe('Bot page health alert', () => {
  it('names the error when the last two real runs failed', async () => {
    daemon.runs = [
      twinRun({ id: 'r3', started_ms: 3000, result: 'failed', error: { code: 'auth' } }),
      twinRun({ id: 'r2', started_ms: 2000, result: 'failed', error: { code: 'auth' } }),
      twinRun({ id: 'r1', started_ms: 1000 }),
    ]
    await load()
    const alert = await screen.findByTestId('twin-health')
    expect(alert).toHaveTextContent('last 2 wakes all failed. The backend rejected its API key.')
    expect(within(alert).getByRole('link', { name: 'Open Bot settings' })).toHaveAttribute('href', '/n/aaaaaaaa/settings?tab=twin')
  })
  it.each([
    ['a single failure', [twinRun({ id: 'r2', started_ms: 2000, result: 'failed' }), twinRun({ id: 'r1', started_ms: 1000 })]],
    ['failures interrupted by a success', [twinRun({ id: 'r3', started_ms: 3000, result: 'failed' }), twinRun({ id: 'r2', started_ms: 2000 }), twinRun({ id: 'r1', started_ms: 1000, result: 'failed' })]],
    ['failed dry runs', [twinRun({ id: 'r2', started_ms: 2000, result: 'failed', dry_run: true }), twinRun({ id: 'r1', started_ms: 1000, result: 'failed', dry_run: true })]],
  ])('stays quiet for %s', async (_name, runs) => {
    daemon.runs = runs
    await load()
    await screen.findByTestId('twin-today')
    expect(screen.queryByTestId('twin-health')).not.toBeInTheDocument()
  })
})

describe('Bot page run history', () => {
  it('groups consecutive idle runs and expands individual decisions', async () => {
    daemon.runs = [
      twinRun({ id: 'r3', started_ms: 3000, result: 'idle', decisions: [], items: [] }),
      twinRun({ id: 'r2', started_ms: 2000, result: 'idle', decisions: [], items: [] }),
      twinRun({ id: 'r1', started_ms: 1000 }),
    ]
    await load()
    expect(await screen.findByRole('button', { name: '2 idle runs' })).toBeInTheDocument()
    expect(screen.getAllByTestId('twin-run')).toHaveLength(1)
    await userEvent.click(screen.getByRole('button', { name: '2 idle runs' }))
    expect(screen.getAllByTestId('twin-run')).toHaveLength(3)
    await userEvent.click(within(screen.getAllByTestId('twin-run')[2]!).getByRole('button'))
    expect(screen.getByText('Ready to build')).toBeInTheDocument()
  })
  it('shows usage on a run line when the run has it, and nothing otherwise', async () => {
    daemon.runs = [
      twinRun({ id: 'r3', started_ms: 3000, usage: { cost_usd: 0.05 } }),
      twinRun({ id: 'r2', started_ms: 2000, usage: { input_tokens: 2500, output_tokens: 40 } }),
      twinRun({ id: 'r1', started_ms: 1000 }),
    ]
    await load()
    await screen.findAllByTestId('twin-run')
    const lines = screen.getAllByTestId('twin-run')
    expect(lines[0]).toHaveTextContent('$0.05')
    expect(lines[1]).toHaveTextContent('2.5k in · 40 out')
    expect(lines[2]).not.toHaveTextContent('$')
    expect(lines[2]).not.toHaveTextContent(' in ·')
  })
  it('no longer has a last-wake line under the history', async () => {
    daemon.runs = [twinRun()]
    await load()
    await screen.findAllByTestId('twin-run')
    expect(screen.queryByText(/^Last wake:/)).not.toBeInTheDocument()
  })
  it('opens a run on an older page outside the selected nodes', async () => {
    daemon.runs = Array.from({ length: 51 }, (_, i) => twinRun({ id: `r${i}`, started_ms: 10000 - i }))
    usePrefs.setState({ sidebarNodes: [] })
    render(<TwinPage node={A} run="r50" />)
    expect(await screen.findByText('Ready to build')).toBeInTheDocument()
    expect(calls('twin.runs').some((c) => (c.params as { before_ms?: number }).before_ms === 9951)).toBe(true)
  })
  it('reports a cleaned up run', async () => {
    render(<TwinPage node={A} run="gone" />)
    expect(await screen.findByText('This run is no longer available (records are kept for 90 days).')).toBeInTheDocument()
  })
})
