// 节点设置 →「限制」（web.md § 4.8，ADR-43）：按 node.get_config 渲染可编辑的表单，保存时一次 node.set_config，错误逐项显示。
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import type { ConfigChange, ConfigItem, NodeSetConfigParams } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { LimitsTab } from './LimitsTab'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc
let items: ConfigItem[]

function fresh(): ConfigItem[] {
  return [
    { key: 'sessions.max_concurrent', value: 3, kind: 'int', editable: 'any', env_override: false, min: 1, max: 32 },
    { key: 'sessions.idle_timeout', value: '20m', kind: 'duration', editable: 'any', env_override: false, min: 0, max: 604_800_000 },
    { key: 'permissions.allow_dangerous_modes', value: true, kind: 'bool', editable: 'tighten_only', env_override: false },
    { key: 'worktree.default', value: 'worktree', kind: 'enum', editable: 'any', env_override: false, options: ['worktree', 'local'] },
    { key: 'worktree.branch_prefix', value: 'anycon', kind: 'text', editable: 'any', env_override: false, max: 40 },
    { key: 'worktree.auto_archive_on_pr_close', value: false, kind: 'bool', editable: 'any', env_override: false },
    { key: 'preview.enabled', value: false, kind: 'bool', editable: 'tighten_only', env_override: false },
    { key: 'log.level', value: 'info', kind: 'enum', editable: 'any', env_override: false, options: ['error', 'warn', 'info', 'debug', 'trace'] },
  ]
}

const setCalls = () => rpc.calls.filter((c) => c.method === 'node.set_config').map((c) => c.params as NodeSetConfigParams)
const getCalls = () => rpc.calls.filter((c) => c.method === 'node.get_config')
const ctl = (key: string) => screen.getByTestId(`limit-${key}`)

function serve(): void {
  rpc.handlers['node.get_config'] = () => ({ items })
  rpc.handlers['node.set_config'] = (p) => {
    for (const ch of (p as NodeSetConfigParams).changes as ConfigChange[]) {
      const it = items.find((i) => i.key === ch.key)!
      items = items.map((i) => (i === it ? { ...i, value: ch.value } : i))
    }
    return { items }
  }
}

function node(conn: 'ready' | 'peer_offline' = 'ready'): void {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn }) }, order: [NODE], loaded: true })
}

beforeEach(() => {
  items = fresh()
  rpc = new FakeRpc()
  setRpc(rpc)
  useUi.setState({ toasts: [] })
  node()
  serve()
})
afterEach(() => setRpc(null))

describe('node settings: the Limits tab', () => {
  it('shows each setting from the node in its section, with the control its type calls for', async () => {
    render(<LimitsTab nodeId={NODE} />)
    expect(await screen.findByTestId('limits-section-sessions')).toHaveTextContent('Sessions')
    for (const s of ['permissions', 'worktree', 'preview', 'log']) expect(screen.getByTestId(`limits-section-${s}`)).toBeInTheDocument()
    expect(getCalls()).toHaveLength(1)

    expect(ctl('sessions.max_concurrent')).toHaveValue(3)
    expect(ctl('sessions.max_concurrent')).toHaveAttribute('type', 'number')
    expect(ctl('sessions.max_concurrent')).toHaveAttribute('min', '1')
    expect(ctl('sessions.max_concurrent')).toHaveAttribute('max', '32')
    expect(screen.getByText('1 to 32')).toBeInTheDocument()
    expect(ctl('sessions.idle_timeout')).toHaveValue('20m')
    expect(screen.getByText(/0 to 7d, written like 30s, 20m or 2h/)).toBeInTheDocument()
    expect(ctl('permissions.allow_dangerous_modes')).toHaveAttribute('aria-checked', 'true')
    expect(ctl('worktree.default')).toHaveValue('worktree')
    expect(within(ctl('worktree.default')).getAllByRole('option').map((o) => o.textContent)).toEqual(['A worktree of its own', 'The project folder'])
    expect(ctl('worktree.branch_prefix')).toHaveValue('anycon')
    expect(ctl('log.level')).toHaveValue('info')
    expect(screen.getByLabelText('Concurrent sessions')).toBe(ctl('sessions.max_concurrent'))
  })

  it('has nothing to save until something changes', async () => {
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    expect(screen.getByTestId('limits-save')).toBeDisabled()
    expect(screen.getByTestId('limits-reset')).toBeDisabled()
    await user.clear(ctl('sessions.max_concurrent'))
    await user.type(ctl('sessions.max_concurrent'), '3')
    expect(screen.getByTestId('limits-save')).toBeDisabled()
  })

  it('saves only what changed, in a single node.set_config, and shows the node’s answer', async () => {
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')

    await user.clear(ctl('sessions.max_concurrent'))
    await user.type(ctl('sessions.max_concurrent'), '8')
    await user.clear(ctl('worktree.branch_prefix'))
    await user.type(ctl('worktree.branch_prefix'), 'team')
    await user.selectOptions(ctl('log.level'), 'debug')
    await user.click(ctl('worktree.auto_archive_on_pr_close'))
    await user.click(screen.getByTestId('limits-save'))

    await waitFor(() => expect(setCalls()).toHaveLength(1))
    expect(setCalls()[0]!.changes).toEqual([
      { key: 'sessions.max_concurrent', value: 8 },
      { key: 'worktree.branch_prefix', value: 'team' },
      { key: 'worktree.auto_archive_on_pr_close', value: true },
      { key: 'log.level', value: 'debug' },
    ])
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.text)).toContain('Settings saved'))
    // 表单显示节点返回的新值，保存按钮又灰了
    expect(ctl('sessions.max_concurrent')).toHaveValue(8)
    expect(ctl('log.level')).toHaveValue('debug')
    expect(screen.getByTestId('limits-save')).toBeDisabled()
  })

  it('a duration is trimmed and sent as written', async () => {
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    await user.clear(ctl('sessions.idle_timeout'))
    await user.type(ctl('sessions.idle_timeout'), ' 45m ')
    await user.click(screen.getByTestId('limits-save'))
    await waitFor(() => expect(setCalls()).toHaveLength(1))
    expect(setCalls()[0]!.changes).toEqual([{ key: 'sessions.idle_timeout', value: '45m' }])
  })

  it('discarding puts the node’s values back and sends nothing', async () => {
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    await user.selectOptions(ctl('worktree.default'), 'local')
    await user.click(ctl('worktree.auto_archive_on_pr_close'))
    await user.click(screen.getByTestId('limits-reset'))
    expect(ctl('worktree.default')).toHaveValue('worktree')
    expect(ctl('worktree.auto_archive_on_pr_close')).toHaveAttribute('aria-checked', 'false')
    expect(screen.getByTestId('limits-save')).toBeDisabled()
    expect(setCalls()).toEqual([])
  })

  it('checks numbers and durations before asking the node, and blocks saving while one is wrong', async () => {
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    await user.clear(ctl('sessions.max_concurrent'))
    await user.type(ctl('sessions.max_concurrent'), '99')
    expect(screen.getByTestId('limit-error-sessions.max_concurrent')).toHaveTextContent('Enter a whole number from 1 to 32.')
    expect(screen.getByTestId('limits-save')).toBeDisabled()
    await user.clear(ctl('sessions.idle_timeout'))
    await user.type(ctl('sessions.idle_timeout'), 'soon')
    expect(screen.getByTestId('limit-error-sessions.idle_timeout')).toHaveTextContent('This value is not valid.')
    await user.clear(ctl('sessions.max_concurrent'))
    await user.type(ctl('sessions.max_concurrent'), '4')
    expect(screen.getByTestId('limits-save')).toBeDisabled()
    await user.clear(ctl('sessions.idle_timeout'))
    await user.type(ctl('sessions.idle_timeout'), '1h')
    expect(screen.queryByTestId('limit-error-sessions.idle_timeout')).toBeNull()
    expect(screen.getByTestId('limits-save')).toBeEnabled()
    expect(setCalls()).toEqual([])
  })

  it('a setting that was already outside its range on the node does not stop saving another one', async () => {
    items = items.map((i) => (i.key === 'sessions.max_concurrent' ? { ...i, value: 50 } : i))
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    expect(ctl('sessions.max_concurrent')).toHaveValue(50)
    expect(screen.queryByTestId('limit-error-sessions.max_concurrent')).toBeNull()
    await user.selectOptions(ctl('log.level'), 'warn')
    await user.click(screen.getByTestId('limits-save'))
    await waitFor(() => expect(setCalls()).toHaveLength(1))
    expect(setCalls()[0]!.changes).toEqual([{ key: 'log.level', value: 'warn' }])
  })

  it('a value the node has that is not among the options is still shown', async () => {
    items = items.map((i) => (i.key === 'log.level' ? { ...i, value: 'INFO' } : i))
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-log')
    expect(ctl('log.level')).toHaveValue('INFO')
  })

  it('a setting this version has no wording for is shown under its key', async () => {
    items = [...items, { key: 'future.flag', value: true, kind: 'bool', editable: 'any', env_override: false }]
    render(<LimitsTab nodeId={NODE} />)
    const row = (await screen.findAllByTestId('limit-row')).find((r) => r.getAttribute('data-key') === 'future.flag')!
    expect(row).toHaveTextContent('future.flag')
    expect(screen.getByTestId('limits-section-future')).toHaveTextContent('future')
  })
})

describe('node settings: the Limits tab and the two security switches', () => {
  it('a switch that is on can be turned off, and the page says that turning it on takes the command line', async () => {
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-permissions')
    expect(ctl('permissions.allow_dangerous_modes')).toBeEnabled()
    expect(screen.getByTestId('limit-tighten-permissions.allow_dangerous_modes')).toHaveTextContent('anycon config set permissions.allow_dangerous_modes true')
    await user.click(ctl('permissions.allow_dangerous_modes'))
    await user.click(screen.getByTestId('limits-save'))
    await waitFor(() => expect(setCalls()).toHaveLength(1))
    expect(setCalls()[0]!.changes).toEqual([{ key: 'permissions.allow_dangerous_modes', value: false }])
    // 关掉之后它就锁住了
    await waitFor(() => expect(ctl('permissions.allow_dangerous_modes')).toBeDisabled())
  })

  it('turning it off and changing your mind before saving is allowed', async () => {
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-permissions')
    await user.click(ctl('permissions.allow_dangerous_modes'))
    expect(ctl('permissions.allow_dangerous_modes')).toHaveAttribute('aria-checked', 'false')
    await user.click(ctl('permissions.allow_dangerous_modes'))
    expect(ctl('permissions.allow_dangerous_modes')).toHaveAttribute('aria-checked', 'true')
    expect(screen.getByTestId('limits-save')).toBeDisabled()
  })

  it('a switch that is off on the node is disabled and names the command to run on the machine', async () => {
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-preview')
    expect(ctl('preview.enabled')).toHaveAttribute('aria-checked', 'false')
    expect(ctl('preview.enabled')).toBeDisabled()
    expect(screen.getByTestId('limit-tighten-preview.enabled')).toHaveTextContent('run `anycon config set preview.enabled true` on the node')
  })
})

describe('node settings: the Limits tab and environment variables', () => {
  it('a setting set by an environment variable is read-only and says which variable', async () => {
    items = items.map((i) => (i.key === 'sessions.max_concurrent' || i.key === 'log.level' ? { ...i, env_override: true, editable: 'no' as const } : i))
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    expect(ctl('sessions.max_concurrent')).toBeDisabled()
    expect(screen.getByTestId('limit-env-sessions.max_concurrent')).toHaveTextContent('ANYCON_SESSIONS__MAX_CONCURRENT')
    expect(ctl('log.level')).toBeDisabled()
    expect(screen.getByTestId('limit-env-log.level')).toHaveTextContent('ANYCON_LOG')
    expect(ctl('sessions.idle_timeout')).toBeEnabled()
    expect(screen.queryByTestId('limit-env-sessions.idle_timeout')).toBeNull()
  })
})

describe('node settings: the Limits tab and errors from the node', () => {
  it('shows the node’s per-setting error and keeps what the user typed', async () => {
    rpc.handlers['node.set_config'] = () => {
      throw rpcError('invalid', 'worktree.branch_prefix', 'format')
    }
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    await user.clear(ctl('worktree.branch_prefix'))
    await user.type(ctl('worktree.branch_prefix'), 'main')
    await user.selectOptions(ctl('log.level'), 'trace')
    await user.click(screen.getByTestId('limits-save'))
    expect(await screen.findByTestId('limit-error-worktree.branch_prefix')).toHaveTextContent('This value is not valid.')
    expect(screen.getByTestId('limits-not-saved')).toHaveTextContent('Nothing was saved')
    expect(screen.queryByTestId('limit-error-log.level')).toBeNull()
    expect(ctl('worktree.branch_prefix')).toHaveValue('main')
    expect(ctl('log.level')).toHaveValue('trace')
    // 改那一项，它的错误就消失
    await user.type(ctl('worktree.branch_prefix'), 'x')
    expect(screen.queryByTestId('limit-error-worktree.branch_prefix')).toBeNull()
  })

  it('a refusal because the setting got stricter on the node is shown, and the list is read again', async () => {
    rpc.handlers['node.set_config'] = () => {
      throw rpcError('forbidden', 'permissions.allow_dangerous_modes', 'tighten_only')
    }
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-permissions')
    await user.click(ctl('permissions.allow_dangerous_modes'))
    // 在保存之前，这个键在节点上已经被人关掉了
    items = items.map((i) => (i.key === 'permissions.allow_dangerous_modes' ? { ...i, value: false } : i))
    await user.click(screen.getByTestId('limits-save'))
    expect(await screen.findByTestId('limit-error-permissions.allow_dangerous_modes')).toHaveTextContent('From here this can only be turned off')
    await waitFor(() => expect(getCalls()).toHaveLength(2))
    await waitFor(() => expect(ctl('permissions.allow_dangerous_modes')).toBeDisabled())
  })

  it('a refusal because an environment variable took the setting over names the variable', async () => {
    rpc.handlers['node.set_config'] = () => {
      throw rpcError('forbidden', 'sessions.max_concurrent', 'env_override')
    }
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    await user.clear(ctl('sessions.max_concurrent'))
    await user.type(ctl('sessions.max_concurrent'), '5')
    await user.click(screen.getByTestId('limits-save'))
    expect(await screen.findByTestId('limit-error-sessions.max_concurrent')).toHaveTextContent('ANYCON_SESSIONS__MAX_CONCURRENT')
  })

  it('an error that is not about one setting is a toast, and the form keeps its edits', async () => {
    rpc.handlers['node.set_config'] = () => {
      throw rpcError('internal')
    }
    const user = userEvent.setup()
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    await user.selectOptions(ctl('log.level'), 'warn')
    await user.click(screen.getByTestId('limits-save'))
    await waitFor(() => expect(useUi.getState().toasts.some((t) => t.kind === 'error' && t.text === 'Something went wrong on the node.')).toBe(true))
    expect(ctl('log.level')).toHaveValue('warn')
    expect(screen.getByTestId('limits-save')).toBeEnabled()
  })

  it('shows why the settings could not be read', async () => {
    rpc.handlers['node.get_config'] = () => {
      throw rpcError('unsupported')
    }
    render(<LimitsTab nodeId={NODE} />)
    expect(await screen.findByTestId('limits-error')).toHaveTextContent('This agent does not support that.')
    expect(screen.queryByTestId('limits-save')).toBeNull()
  })
})

describe('node settings: the Limits tab and an offline node', () => {
  it('does not ask an offline node and says so', async () => {
    node('peer_offline')
    render(<LimitsTab nodeId={NODE} />)
    expect(await screen.findByTestId('limits-offline')).toBeInTheDocument()
    expect(rpc.methods()).toEqual([])
    expect(screen.queryByTestId('limits-save')).toBeNull()
  })

  it('everything is disabled when the node goes offline after the settings were loaded', async () => {
    render(<LimitsTab nodeId={NODE} />)
    await screen.findByTestId('limits-section-sessions')
    node('peer_offline')
    await waitFor(() => expect(ctl('sessions.max_concurrent')).toBeDisabled())
    expect(ctl('log.level')).toBeDisabled()
    expect(ctl('worktree.auto_archive_on_pr_close')).toBeDisabled()
    expect(screen.getByTestId('limits-save')).toBeDisabled()
    expect(screen.getByTestId('limits-offline')).toBeInTheDocument()
  })
})
