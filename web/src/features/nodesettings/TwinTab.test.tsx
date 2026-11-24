// 节点设置 →「Bot」：标签出现的条件、状态行、判断后端（和「限制」分工）、范围与首次流程、被别的节点使用、离线。
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest'
import type { ConfigChange, ConfigItem, NodeInfo, NodeSetConfigParams, PeerDevice, TwinSetParams } from '@/protocol'
import { mockAgent, nodeEntry, project } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTwinDaemon, twinRun, twinValue } from '@/test/fakeTwinDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { useTwin } from '@/features/twin/store'
import { NodeSettingsPage } from '@/pages/NodeSettingsPage'
import { LimitsTab } from './LimitsTab'
import { TwinTab } from './TwinTab'

const NODE = 'n'.repeat(32)
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), { node: 'n'.repeat(32) }))
vi.mock('@/components/PageHeader', () => ({ PageHeader: () => null }))
vi.mock('@/features/preview/availability', () => ({ usePreviewTemplate: () => null, previewAvailable: () => false }))

let rpc: FakeRpc
let daemon: FakeTwinDaemon
let items: ConfigItem[]
const info = (over: Partial<NodeInfo> = {}, twin = true): NodeInfo =>
  ({ capabilities: { twin, todo: true }, twin_accept_links: true, judge: { triage: 'mock', backends: [{ name: 'mock' }], daily_calls: 20 }, ...over }) as unknown as NodeInfo
const device = (over: Partial<PeerDevice> = {}): PeerDevice => ({ device_id: 'dev-twin', fingerprint: 'abcd', kind: 'twin', name: 'beta bot', online: true, paired_at_ms: 1, ...over })

function setNode(over: Partial<Parameters<typeof nodeEntry>[0]> = {}): void {
  useNodes.setState({
    byId: {
      [NODE]: nodeEntry({
        id: NODE,
        name: 'alpha',
        info: info(),
        projects: [project({ name: 'web' }), project({ name: 'api' })],
        agents: [mockAgent({ modes: [{ id: 'ask', label: 'Ask', risk: 'normal' }] })],
        ...over,
      }),
    },
    order: [NODE],
    loaded: true,
  })
}
const calls = (method: string) => rpc.calls.filter((c) => c.method === method)
const setConfigCalls = () => calls('node.set_config').map((c) => c.params as NodeSetConfigParams)
const twinSets = () => calls('twin.set').map((c) => c.params as TwinSetParams)

beforeEach(() => {
  rpc = new FakeRpc()
  daemon = new FakeTwinDaemon(rpc)
  setRpc(rpc)
  items = [
    { key: 'sessions.max_concurrent', value: 4, kind: 'int', editable: 'any', min: 1, max: 32 },
    { key: 'judge.triage', value: 'mock', kind: 'enum', editable: 'any', options: ['', 'mock', 'other'] },
    { key: 'judge.daily_calls', value: 20, kind: 'int', editable: 'any', min: 0, max: 1000 },
    { key: 'twin.accept_links', value: true, kind: 'bool', editable: 'tighten_only' },
  ]
  rpc.handlers['node.get_config'] = () => ({ items })
  rpc.handlers['node.set_config'] = (p) => {
    for (const ch of (p as NodeSetConfigParams).changes as ConfigChange[]) items = items.map((i) => (i.key === ch.key ? { ...i, value: ch.value } : i))
    return { items }
  }
  rpc.handlers['node.devices'] = () => ({ devices: [device(), { ...device(), device_id: 'dev-web', kind: 'web', name: 'my laptop' }] })
  rpc.handlers['node.info'] = () => info()
  useTwin.getState().reset()
  useUi.setState({ toasts: [] })
  setNode()
})
afterEach(() => setRpc(null))

describe('Settings page: the Bot tab', () => {
  it('exists only on nodes that support Bot, between Limits and the rest', async () => {
    render(<NodeSettingsPage tab="twin" />)
    expect(await screen.findByTestId('twin-tab')).toBeInTheDocument()
    const order = screen.getAllByRole('tab').map((t) => t.textContent)
    expect(order.indexOf('Bot')).toBe(order.indexOf('Limits') + 1)
  })

  it('falls back to the first tab for ?tab=twin on a node without Bot', async () => {
    setNode({ info: info({}, false) })
    render(<NodeSettingsPage tab="twin" />)
    expect(screen.queryByTestId('tab-twin')).toBeNull()
    expect(screen.getByTestId('tab-agent')).toHaveAttribute('data-state', 'active')
    expect(screen.queryByTestId('twin-tab')).toBeNull()
  })
})

describe('Bot tab: status line', () => {
  const state = () => screen.getByTestId('twin-tab-state')
  it('shows the daemon state and links to the run status page', async () => {
    daemon.values.set(NODE, twinValue({ state: 'paused' }))
    render(<TwinTab nodeId={NODE} />)
    await waitFor(() => expect(state()).toHaveTextContent('Paused'))
    expect(screen.getByTestId('twin-tab-runs-link')).toHaveAttribute('href', '/twin/nnnnnnnn')
  })

  it('says not configured even when also paused, and names what is missing', async () => {
    setNode({ info: info({ judge: { triage: 'mock', backends: [{ name: 'mock', problem: 'x' }], daily_calls: 20 } } as unknown as Partial<NodeInfo>) })
    daemon.values.set(NODE, twinValue({ state: 'paused', configured: false, scope: { paused: true, projects: [], max_sessions: 2, daily_dispatches: 10 } }))
    render(<TwinTab nodeId={NODE} />)
    await waitFor(() => expect(state()).toHaveTextContent('Not configured'))
    expect(state()).not.toHaveTextContent('Paused')
    const missing = screen.getByTestId('twin-tab-missing')
    expect(within(missing).getByText(/No projects selected/)).toBeInTheDocument()
    expect(within(missing).getByText(/No usable triage backend/)).toBeInTheDocument()
  })

  it('explains a machine-disabled bot and an offline node', async () => {
    daemon.values.set(NODE, twinValue({ enabled: false, state: 'disabled' }))
    const { unmount } = render(<TwinTab nodeId={NODE} />)
    await waitFor(() => expect(state()).toHaveTextContent('switched off on this machine'))
    unmount()
    setNode({ conn: 'peer_offline' })
    render(<TwinTab nodeId={NODE} />)
    expect(state()).toHaveTextContent('Node offline')
  })
})

describe('Bot tab: judge backends', () => {
  it('shows judge.* and twin.* items, saves only the changed ones and refreshes node.info', async () => {
    const user = userEvent.setup()
    render(<TwinTab nodeId={NODE} />)
    await screen.findByTestId('limit-judge.triage')
    expect(screen.queryByTestId('limit-sessions.max_concurrent')).toBeNull()
    expect(screen.getByLabelText('Daily judgment calls')).toHaveValue(20)
    expect(screen.getByText(/shared by|share these backends/i)).toBeInTheDocument()
    await user.selectOptions(screen.getByTestId('limit-judge.triage'), 'other')
    await user.click(screen.getByTestId('twin-config-save'))
    await waitFor(() => expect(setConfigCalls()).toEqual([{ changes: [{ key: 'judge.triage', value: 'other' }] }]))
    await waitFor(() => expect(calls('node.info')).toHaveLength(1))
  })

  it('refreshes node.info after a twin.* key is saved', async () => {
    const user = userEvent.setup()
    render(<TwinTab nodeId={NODE} />)
    await user.click(await screen.findByTestId('limit-twin.accept_links'))
    await user.click(screen.getByTestId('twin-config-save'))
    await waitFor(() => expect(setConfigCalls()).toEqual([{ changes: [{ key: 'twin.accept_links', value: false }] }]))
    await waitFor(() => expect(calls('node.info')).toHaveLength(1))
  })

  it('says why a failed load happened, next to Retry', async () => {
    rpc.handlers['twin.get'] = () => {
      throw new Error('boom')
    }
    render(<TwinTab nodeId={NODE} />)
    const alert = await screen.findByRole('alert')
    expect(alert).toHaveTextContent('Could not load')
    expect(within(alert).getByRole('button', { name: 'Retry' })).toBeInTheDocument()
  })

  it('keeps a tighten-only switch that is already off disabled', async () => {
    items = items.map((i) => (i.key === 'twin.accept_links' ? { ...i, value: false } : i))
    render(<TwinTab nodeId={NODE} />)
    expect(await screen.findByTestId('limit-twin.accept_links')).toBeDisabled()
    expect(screen.getByTestId('limit-tighten-twin.accept_links')).toBeInTheDocument()
  })

  it('moves out of Limits when the node supports Bot, and stays in Limits when it does not', async () => {
    const { unmount } = render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limit-sessions.max_concurrent')
    expect(screen.queryByTestId('limit-judge.triage')).toBeNull()
    expect(screen.queryByTestId('limit-twin.accept_links')).toBeNull()
    unmount()
    setNode({ info: info({}, false) })
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limit-judge.triage')
    expect(screen.getByTestId('limit-twin.accept_links')).toBeInTheDocument()
  })
})

describe('Bot tab: scope and first-time flow', () => {
  const saveScope = async (user: ReturnType<typeof userEvent.setup>) => {
    const save = await screen.findByRole('button', { name: 'Save scope' })
    await waitFor(() => expect(save).toBeEnabled())
    await user.click(save)
  }

  it('loads the bot itself, then saves a first configuration paused, shows the plan and "Do it" unpauses and wakes', async () => {
    daemon.values.set(NODE, twinValue({ configured: false, state: 'not_configured', scope: { paused: false, projects: [], max_sessions: 2, daily_dispatches: 10 } }))
    daemon.dry = twinRun({ id: 'dry-1', dry_run: true, result: 'done' })
    const user = userEvent.setup()
    render(<TwinTab nodeId={NODE} />)
    await user.click(await screen.findByLabelText('web'))
    await saveScope(user)
    await waitFor(() => expect(twinSets()[0]).toMatchObject({ projects: ['web'], paused: true }))
    expect(calls('twin.dry_run')).toHaveLength(1)
    expect(await screen.findByText('Ready to build')).toBeInTheDocument()
    await user.click(screen.getByTestId('twin-tab-do-it'))
    await waitFor(() => expect(calls('twin.wake')).toHaveLength(1))
    expect(twinSets().at(-1)).toEqual({ paused: false })
  })

  it('does not offer "Do it" when the dry run failed', async () => {
    daemon.values.set(NODE, twinValue({ configured: false, state: 'not_configured', scope: { paused: false, projects: [], max_sessions: 2, daily_dispatches: 10 } }))
    daemon.dry = twinRun({ id: 'dry-2', dry_run: true, result: 'failed' })
    const user = userEvent.setup()
    render(<TwinTab nodeId={NODE} />)
    await user.click(await screen.findByLabelText('web'))
    await saveScope(user)
    await screen.findByTestId('twin-tab-do-it')
    expect(screen.getByTestId('twin-tab-do-it')).toBeDisabled()
  })

  it('just saves when the bot is already configured', async () => {
    daemon.values.set(NODE, twinValue())
    const user = userEvent.setup()
    render(<TwinTab nodeId={NODE} />)
    await user.click(await screen.findByLabelText('api'))
    await saveScope(user)
    await waitFor(() => expect(twinSets()).toHaveLength(1))
    expect(twinSets()[0]).toMatchObject({ projects: ['web', 'api'] })
    expect(twinSets()[0]).not.toHaveProperty('paused')
    expect(calls('twin.dry_run')).toHaveLength(0)
    expect(screen.queryByTestId('twin-tab-do-it')).toBeNull()
  })
})

describe('Bot tab: used by other nodes', () => {
  it('lists only bot devices and opens the scope dialog', async () => {
    daemon.values.set(NODE, twinValue())
    const user = userEvent.setup()
    render(<TwinTab nodeId={NODE} />)
    const list = await screen.findByTestId('twin-tab-devices')
    await waitFor(() => expect(within(list).getByText('beta bot')).toBeInTheDocument())
    expect(within(list).queryByText('my laptop')).toBeNull()
    expect(screen.getByTestId('twin-tab-devices-link')).toHaveAttribute('href', '/n/nnnnnnnn/settings?tab=devices')
    await user.click(screen.getByTestId('twin-tab-device-scope'))
    expect(await screen.findByTestId('twin-scope-dialog')).toBeInTheDocument()
    expect(calls('twin.get').some((c) => (c.params as { requester?: string }).requester === 'dev-twin')).toBe(true)
  })

  it('shows an empty state, and a note when this machine does not accept bot links', async () => {
    rpc.handlers['node.devices'] = () => ({ devices: [] })
    setNode({ info: info({ twin_accept_links: false }) })
    render(<TwinTab nodeId={NODE} />)
    expect(await screen.findByText('No other node’s Bot is paired with this node.')).toBeInTheDocument()
    expect(screen.getByTestId('twin-tab-accept-off')).toHaveTextContent('anycon config set twin.accept_links true')
    expect(screen.getByTestId('twin-tab-accept-off')).not.toHaveTextContent('above')
  })
})

describe('Bot tab: offline', () => {
  it('reads and writes nothing, and disables the controls', async () => {
    setNode({ conn: 'peer_offline', devices: [device()] })
    useTwin.getState().patch(NODE, { value: twinValue() })
    render(<TwinTab nodeId={NODE} />)
    expect(screen.getByTestId('twin-config-offline')).toBeInTheDocument()
    expect(screen.getByTestId('twin-tab-device-scope')).toBeDisabled()
    expect(screen.getByTestId('twin-tab-dry-run')).toBeDisabled()
    expect(screen.getByRole('button', { name: 'Save scope' })).toBeDisabled()
    expect(rpc.calls).toHaveLength(0)
  })
})
