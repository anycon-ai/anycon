// 节点设置 → Agent 卡片里的「Agent 设置」：按 agent 的配置键（agents.<id>.*，ADR-64），和「限制」同一个表单、同一对
// node.get_config / node.set_config；每张卡片只显示自己那个 agent 的键，和新会话的默认选项分开保存。
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import type { ConfigChange, ConfigItem, NodeSetConfigParams } from '@/protocol'
import { codexAgent, mockAgent, nodeEntry } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { AgentTab } from './AgentTab'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc
let items: ConfigItem[]

const timeout = (key: string, value: string): ConfigItem => ({ key, value, kind: 'duration', editable: 'any', env_override: false, min: 10_000, max: 3_600_000 })

beforeEach(() => {
  items = [
    { key: 'sessions.max_concurrent', value: 3, kind: 'int', editable: 'any', env_override: false, min: 1, max: 32 },
    timeout('agents.mock.start_timeout', '2m'),
    timeout('agents.codex.start_timeout', '5m'),
    timeout('agents.acp.opencode.start_timeout', '2m'),
  ]
  rpc = new FakeRpc()
  setRpc(rpc)
  rpc.handlers['node.get_config'] = () => ({ items })
  rpc.handlers['node.set_config'] = (p) => {
    for (const ch of (p as NodeSetConfigParams).changes as ConfigChange[]) items = items.map((i) => (i.key === ch.key ? { ...i, value: ch.value } : i))
    return { items }
  }
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent(), codexAgent()] }) }, order: [NODE], loaded: true })
})
afterEach(() => setRpc(null))

const card = (id: string) => screen.getAllByTestId('agent-card').find((c) => c.querySelector(`[data-agent="${id}"]`))!

describe('node settings: per-agent settings on the Agent tab', () => {
  it('each card shows only its own agent’s settings, with the value in effect and the allowed range', async () => {
    render(<AgentTab nodeId={NODE} />)
    const mock = await within(card('mock')).findByTestId('limit-agents.mock.start_timeout')
    expect(mock).toHaveValue('2m')
    expect(within(card('mock')).getByText('Agent settings')).toBeInTheDocument()
    expect(within(card('mock')).getByLabelText('Start timeout')).toBe(mock)
    expect(within(card('mock')).getByText(/10s to 1h/)).toBeInTheDocument()
    expect(within(card('codex')).getByTestId('limit-agents.codex.start_timeout')).toHaveValue('5m')
    // 别的 agent 的键、节点级的键都不在卡片里
    expect(within(card('mock')).queryByTestId('limit-agents.codex.start_timeout')).toBeNull()
    expect(screen.queryByTestId('limit-sessions.max_concurrent')).toBeNull()
    expect(screen.queryByTestId('limit-agents.acp.opencode.start_timeout')).toBeNull()
  })

  it('saves with node.set_config, apart from the new-session defaults', async () => {
    const user = userEvent.setup()
    render(<AgentTab nodeId={NODE} />)
    const input = await within(card('mock')).findByTestId('limit-agents.mock.start_timeout')
    expect(screen.getByTestId('agent-config-mock-save')).toBeDisabled()
    await user.clear(input)
    await user.type(input, '5m')
    expect(screen.getByTestId('agent-config-mock-save')).toBeEnabled()
    expect(within(card('mock')).getByTestId('agent-defaults-save')).toBeDisabled()
    await user.click(screen.getByTestId('agent-config-mock-save'))
    await waitFor(() => expect(rpc.methods()).toContain('node.set_config'))
    expect(rpc.calls.find((c) => c.method === 'node.set_config')?.params).toEqual({ changes: [{ key: 'agents.mock.start_timeout', value: '5m' }] })
    expect(rpc.methods()).not.toContain('node.set_agent_defaults')
    await waitFor(() => expect(screen.getByTestId('agent-config-mock-save')).toBeDisabled())
  })

  it('refuses a value outside the range before sending anything', async () => {
    const user = userEvent.setup()
    render(<AgentTab nodeId={NODE} />)
    const input = await within(card('mock')).findByTestId('limit-agents.mock.start_timeout')
    await user.clear(input)
    await user.type(input, '5s')
    expect(within(card('mock')).getByRole('alert')).toHaveTextContent('between 10s and 1h')
    expect(screen.getByTestId('agent-config-mock-save')).toBeDisabled()
  })

  it('shows nothing extra when the node lists no settings for the agent (an older daemon)', async () => {
    items = [{ key: 'sessions.max_concurrent', value: 3, kind: 'int', editable: 'any', env_override: false, min: 1, max: 32 }]
    render(<AgentTab nodeId={NODE} />)
    await waitFor(() => expect(rpc.methods()).toContain('node.get_config'))
    expect(screen.queryByText('Agent settings')).toBeNull()
    expect(screen.queryByTestId('agent-config-mock-tab')).toBeNull()
    expect(within(card('mock')).getByTestId('agent-defaults-save')).toBeInTheDocument()
  })
})
