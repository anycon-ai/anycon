// 分身状态、暂停、首次试跑、运行折叠和范围表单，全部通过假节点。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { nodeEntry, mockAgent, project } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTwinDaemon, twinRun, twinValue } from '@/test/fakeTwinDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useTwin } from '@/features/twin/store'
import { syncTwin } from '@/features/twin/service'
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/twin' }))
import { TwinPage } from './TwinPage'
const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
let rpc: FakeRpc
let daemon: FakeTwinDaemon
const info = (twin: boolean): NodeInfo => ({ capabilities: { twin, todo: true }, judge: { triage: 'triage', backends: [], daily_calls: 20 } }) as unknown as NodeInfo
beforeEach(() => {
  rpc = new FakeRpc()
  daemon = new FakeTwinDaemon(rpc)
  setRpc(rpc)
  useTwin.getState().reset()
  usePrefs.setState({ sidebarNodes: null, sidebarSpace: null })
  useNodes.setState({
    byId: {
      [A]: nodeEntry({
        id: A,
        name: 'alpha',
        info: info(true),
        projects: [project({ name: 'web' }), project({ name: 'api' })],
        agents: [
          mockAgent({
            modes: [
              { id: 'ask', label: 'Ask', risk: 'normal' },
              { id: 'auto', label: 'Auto', risk: 'normal', unattended: true },
              { id: 'danger', label: 'Dangerous', risk: 'dangerous' },
            ],
          }),
        ],
      }),
    },
    order: [A],
    loaded: true,
  })
})
async function load() {
  render(<TwinPage />)
  await screen.findAllByText('Scope and budget')
  await waitFor(() => expect(screen.getAllByRole('button', { name: 'Save scope' })[0]).toBeEnabled())
}
const calls = (method: string) => rpc.calls.filter((c) => c.method === method)
describe('Twin page', () => {
  it.each(['idle', 'running', 'paused', 'budget', 'not_configured', 'disabled'] as const)('renders daemon state %s', async (state) => {
    daemon.values.set(A, twinValue({ state, enabled: state !== 'disabled', configured: state !== 'not_configured' }))
    await load()
    expect(screen.getByTestId('twin-state')).toHaveTextContent(
      { idle: 'On', running: 'On · judging', paused: 'Paused', budget: 'Budget used up', not_configured: 'Not configured', disabled: 'Off' }[state],
    )
    if (state === 'disabled') expect(screen.getByText(/Set \[twin\] enabled/)).toBeInTheDocument()
  })
  it('shows offline and hides unsupported nodes and actions', async () => {
    useNodes.getState().patch(A, { conn: 'peer_offline' })
    render(<TwinPage />)
    expect(screen.getByText('Node offline')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Pause' })).toBeDisabled()
    expect(calls('twin.get')).toHaveLength(0)
    act(() => useNodes.getState().patch(A, { info: info(false) }))
    expect(screen.queryByTestId('twin-card')).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Pause' })).not.toBeInTheDocument()
  })
  it('pauses one node, or all nodes in the selected space', async () => {
    useNodes.setState((s) => ({ byId: { ...s.byId, [B]: nodeEntry({ id: B, name: 'beta', info: info(true) }) }, order: [A, B] }))
    await load()
    const alpha = screen.getAllByTestId('twin-card')[0]!
    await userEvent.click(within(alpha).getByRole('button', { name: 'Pause' }))
    await waitFor(() => expect(daemon.values.get(A)?.scope.paused).toBe(true))
    await userEvent.click(screen.getByRole('button', { name: 'Pause all' }))
    await waitFor(() => expect(daemon.values.get(B)?.scope.paused).toBe(true))
    expect(calls('twin.set').every((c) => (c.params as { paused: boolean }).paused)).toBe(true)
  })
  it('dry runs list actions and skipped reasons, then explicitly wake', async () => {
    daemon.dry = twinRun({
      dry_run: true,
      decisions: [{ todo_id: 't1', action: 'dispatch', reason: 'Need more slots', rules: [], outcome: 'skipped', skip_reason: 'no_capacity' }],
    })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Try a dry run' }))
    expect(await screen.findByText('Need more slots')).toBeInTheDocument()
    expect(screen.getByText('Skipped: No available session slots')).toBeInTheDocument()
    expect(calls('twin.wake')).toHaveLength(0)
    await userEvent.click(screen.getByRole('button', { name: 'Do it this way' }))
    await waitFor(() => expect(calls('twin.wake')).toHaveLength(1))
    expect(calls('twin.set')[0]?.params).toEqual({ paused: false })
  })
  it('shows the complete dry-run plan as agent-written plain text', async () => {
    daemon.dry = twinRun({
      dry_run: true,
      decisions: [
        { todo_id: 't1', action: 'dispatch', reason: 'Ready', rules: [], outcome: 'executed', note: '<script>send()</script>\nKeep the API stable.' },
        { todo_id: 't2', action: 'ask_user', reason: 'Need a preference', rules: [], outcome: 'executed', question: '<b>Which platform?</b>', options: ['<img src=x onerror=send()>', '**Web**'] },
        { todo_id: 't3', action: 'ask_advisor', reason: 'Need technical advice', rules: [], outcome: 'executed', question: '[Which index?](https://example.com)' },
      ],
    })
    await load()
    await userEvent.click(screen.getByRole('button', { name: 'Try a dry run' }))
    expect(await screen.findByText('Twin’s supplementary instructions to the agent')).toBeInTheDocument()
    expect(screen.getByText('<script>send()</script> Keep the API stable.')).toBeInTheDocument()
    expect(screen.getByText('<b>Which platform?</b>')).toBeInTheDocument()
    expect(screen.getByText('<img src=x onerror=send()>')).toBeInTheDocument()
    expect(screen.getByText('**Web**')).toBeInTheDocument()
    expect(screen.getByText('[Which index?](https://example.com)')).toBeInTheDocument()
    for (const row of screen.getAllByTestId('twin-decision')) {
      expect(row.querySelector('script, img, b, strong')).toBeNull()
      expect(row.querySelectorAll('a')).toHaveLength(1)
    }
    expect(screen.getAllByText('Written by an agent')).toHaveLength(1)
    expect(calls('twin.answer')).toHaveLength(0)
  })
  it('first setup saves paused and requires a dry run before enabling', async () => {
    daemon.values.set(A, twinValue({ configured: false, state: 'not_configured', scope: { paused: false, projects: [], max_sessions: 2, daily_dispatches: 10 } }))
    await load()
    expect(screen.getByText(/No projects selected/)).toBeInTheDocument()
    await userEvent.click(screen.getByRole('checkbox', { name: 'web' }))
    await userEvent.click(screen.getByRole('button', { name: 'Save scope' }))
    await screen.findByRole('button', { name: 'Do it this way' })
    expect(calls('twin.set')[0]?.params).toMatchObject({ projects: ['web'], paused: true })
    expect(calls('twin.dry_run')).toHaveLength(1)
    expect(calls('twin.wake')).toHaveLength(0)
  })
  it('groups consecutive idle runs and expands individual decisions', async () => {
    daemon.runs = [
      twinRun({ id: 'r3', started_ms: 3000, result: 'idle', decisions: [], items: [] }),
      twinRun({ id: 'r2', started_ms: 2000, result: 'idle', decisions: [], items: [] }),
      twinRun({ id: 'r1', started_ms: 1000 }),
    ]
    await load()
    expect(screen.getByRole('button', { name: '2 idle runs' })).toBeInTheDocument()
    expect(screen.getAllByTestId('twin-run')).toHaveLength(1)
    await userEvent.click(screen.getByRole('button', { name: '2 idle runs' }))
    expect(screen.getAllByTestId('twin-run')).toHaveLength(3)
    await userEvent.click(within(screen.getAllByTestId('twin-run')[2]!).getByRole('button'))
    expect(screen.getByText('Ready to build')).toBeInTheDocument()
  })
  it('filters dangerous modes, marks unattended default, validates max sessions, saves and reports errors', async () => {
    await load()
    expect(screen.getByRole('combobox', { name: 'Mode' })).toBeDisabled()
    await userEvent.selectOptions(screen.getByRole('combobox', { name: 'Agent' }), 'mock')
    expect(screen.queryByRole('option', { name: 'Dangerous' })).not.toBeInTheDocument()
    expect(screen.getByRole('option', { name: 'Auto · Unattended · default' })).toBeInTheDocument()
    expect(screen.getByText(/Choose 1–3 sessions/)).toBeInTheDocument()
    const max = screen.getByRole('spinbutton', { name: 'Concurrent sessions' })
    await userEvent.clear(max)
    await userEvent.type(max, '4')
    expect(screen.getByRole('button', { name: 'Save scope' })).toBeDisabled()
    await userEvent.clear(max)
    await userEvent.type(max, '1')
    await userEvent.click(screen.getByRole('checkbox', { name: 'api' }))
    await userEvent.selectOptions(screen.getByRole('combobox', { name: 'Mode' }), 'auto')
    await userEvent.click(screen.getByRole('button', { name: 'Save scope' }))
    await waitFor(() => expect(calls('twin.set')[0]?.params).toMatchObject({ projects: ['web', 'api'], mode: 'auto', max_sessions: 1 }))
    daemon.failSet = true
    await waitFor(() => expect(screen.getByRole('button', { name: 'Save scope' })).toBeEnabled())
    await userEvent.click(screen.getByRole('button', { name: 'Save scope' }))
    expect(await screen.findByRole('alert')).toBeInTheDocument()
  })
  it('explains a missing triage backend', async () => {
    daemon.values.set(A, twinValue({ configured: false }))
    await load()
    expect(screen.getByText('No available triage backend. Configure one in node settings.')).toBeInTheDocument()
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
  it('only includes selected nodes', async () => {
    useNodes.setState((s) => ({ byId: { ...s.byId, [B]: nodeEntry({ id: B, name: 'beta', info: info(true) }) }, order: [A, B] }))
    usePrefs.setState({ sidebarNodes: [B] })
    await syncTwin(B)
    render(<TwinPage />)
    expect(screen.queryByText('alpha')).not.toBeInTheDocument()
    expect(screen.getByText('beta')).toBeInTheDocument()
  })
})

it('cannot enable Twin from a failed dry run', async () => {
  daemon.dry = twinRun({ result: 'failed', error: { code: 'internal' } })
  await load()
  await userEvent.click(screen.getByRole('button', { name: 'Try a dry run' }))
  expect(await screen.findByRole('button', { name: 'Do it this way' })).toBeDisabled()
  expect(calls('twin.wake')).toHaveLength(0)
})
it('each referenced rule links to its own node and rule', async () => {
  daemon.dry = twinRun({ decisions: [{ todo_id: 't1', action: 'dispatch', reason: 'Ready', rules: ['rule1', 'rule2'], outcome: 'executed' }] })
  await load()
  await userEvent.click(screen.getByRole('button', { name: 'Try a dry run' }))
  expect(await screen.findByRole('link', { name: 'Rules: rule1' })).toHaveAttribute('href', '/rules?node=aaaaaaaa&rule=rule1')
  expect(screen.getByRole('link', { name: 'Rules: rule2' })).toHaveAttribute('href', '/rules?node=aaaaaaaa&rule=rule2')
})
