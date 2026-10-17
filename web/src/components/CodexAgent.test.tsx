// Codex（app-server）形状的 AgentInfo / 事件在通用组件里的渲染核对。
// 渲染层是 AgentInfo 驱动的，不认识具体 agent；这里用 Codex 的真实形状证明这一点：
// 四档权限模式（含 dangerous）、select + bool 选项、按模型不同的思考等级、not_authenticated + `codex login`、
// 流式命令输出（tool_output_delta）、计划（plan）、declined 的工具调用。
import { useState } from 'react'
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { resolveDefaults, toSessionSettings, type Selection } from '@/features/settings/resolve'
import { buildView } from '@/features/session/reducer'
import { exportMarkdown } from '@/features/session/export'
import { DiffView } from '@/features/session/ui/DiffView'
import { MessageList, retryableErrorKey, type MessageListProps } from '@/features/session/ui/MessageList'
import { PermissionCard } from '@/features/session/ui/PermissionCard'
import { AgentTab } from '@/features/nodesettings/AgentTab'
import i18n from '@/i18n'
import { codexAgent, codexNotAuthenticated, ev, evs, mockAgent, nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useDrafts } from '@/stores/drafts'
import { AgentIssueCard, NoAgentsNotice } from './AgentIssueCards'
import { NewSessionForm } from './NewSessionForm'
import { SettingsControls } from './SettingsControls'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

beforeEach(() => {
  resetSeq()
  navigate.mockReset()
  rpc = new FakeRpc()
  setRpc(rpc)
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {} })
  usePrefs.setState({ lastUsed: {}, customModels: {} })
})
afterEach(() => setRpc(null))

function Controls({ agent = codexAgent(), onChange, layout = 'inline', idPrefix = 'option', initial }: { agent?: ReturnType<typeof codexAgent>; onChange?: (s: Selection) => void; layout?: 'inline' | 'stacked'; idPrefix?: string; initial?: Partial<Selection> }) {
  const [sel, setSel] = useState<Selection>({ ...resolveDefaults(agent, []), ...initial })
  return (
    <SettingsControls
      agent={agent}
      value={sel}
      onChange={(n) => {
        setSel(n)
        onChange?.(n)
      }}
      customKey={`n:${agent.id}`}
      idPrefix={idPrefix}
      layout={layout}
    />
  )
}

type User = ReturnType<typeof userEvent.setup>

/** 思考等级是下拉：打开之后才有各个等级项（`<prefix>-thinking-<id>`，选中的 `aria-checked=true`）。 */
async function openThinking(user: User, prefix = 'option'): Promise<void> {
  await user.click(screen.getByTestId(`${prefix}-thinking`))
  await screen.findByRole('menu')
}
/** 当前打开的菜单里各等级项的 id 和选中状态。 */
const thinkingItems = (prefix = 'option'): Array<[string, string | null]> =>
  within(screen.getByRole('menu'))
    .getAllByRole('menuitemradio')
    .map((el) => [(el.getAttribute('data-testid') ?? '').replace(`${prefix}-thinking-`, ''), el.getAttribute('aria-checked')])

describe('Codex: settings controls (new-session form, info panel, node defaults)', () => {
  it('defaults to the flagged model, its own default thinking level, the normal default mode and the declared option defaults', () => {
    const sel = resolveDefaults(codexAgent(), [])
    expect(sel).toEqual({ model: 'codex-large', thinking: 'medium', mode: 'workspace', options: { fast_mode: false, plan_mode: false, model_reasoning_summary: 'auto' } })
    expect(toSessionSettings(sel)).toEqual({ model: 'codex-large', thinking: 'medium', mode: 'workspace', options: sel.options })
  })

  it('shows the model, the thinking levels of that model and the mode', async () => {
    const user = userEvent.setup()
    render(<Controls />)
    expect(screen.getByTestId('option-model')).toHaveTextContent('Codex Large')
    expect(screen.getByTestId('option-thinking')).toHaveAttribute('data-value', 'medium')
    await openThinking(user)
    expect(thinkingItems()).toEqual([
      ['low', 'false'],
      ['medium', 'true'],
      ['high', 'false'],
      ['xhigh', 'false'],
    ])
    await user.keyboard('{Escape}')
    expect(screen.getByTestId('option-mode')).toHaveTextContent('Workspace')
    expect(screen.getByTestId('option-mode')).toHaveAttribute('data-risk', 'normal')
    expect(screen.getByTestId('option-more')).toBeInTheDocument()
  })

  it('offers different thinking levels per model, adjusts a level the new model lacks, and hides the control for a model without any', async () => {
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Controls onChange={onChange} initial={{ thinking: 'xhigh' }} />)
    await user.click(screen.getByTestId('option-model'))
    await user.click(await screen.findByTestId('option-model-codex-mini'))
    // xhigh 不在 mini 里：回落到 mini 的默认 low
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ model: 'codex-mini', thinking: 'low' }))
    expect(screen.getByTestId('option-thinking')).toHaveAttribute('data-value', 'low')
    await openThinking(user)
    expect(screen.queryByTestId('option-thinking-xhigh')).toBeNull()
    expect(screen.getByTestId('option-thinking-minimal')).toBeInTheDocument()
    expect(screen.getByTestId('option-thinking-low')).toHaveAttribute('aria-checked', 'true')
    await user.keyboard('{Escape}')

    await user.click(screen.getByTestId('option-model'))
    await user.click(await screen.findByTestId('option-model-codex-plain'))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ model: 'codex-plain', thinking: null }))
    expect(screen.queryByTestId('option-thinking')).toBeNull()
  })

  it('lists all four modes with their risk; safe applies at once, dangerous asks first and is shown in red', async () => {
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Controls onChange={onChange} />)
    await user.click(screen.getByTestId('option-mode'))
    for (const id of ['read_only', 'workspace', 'auto_review', 'full_access']) expect(await screen.findByTestId(`option-mode-${id}`)).toBeInTheDocument()
    expect(screen.getByTestId('option-mode-workspace')).toHaveTextContent('default')
    expect(screen.getByTestId('option-mode-full_access')).toHaveTextContent('No sandbox, no approvals')

    await user.click(screen.getByTestId('option-mode-read_only'))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ mode: 'read_only' }))
    expect(screen.getByTestId('option-mode')).toHaveAttribute('data-risk', 'safe')

    await user.click(screen.getByTestId('option-mode'))
    await user.click(await screen.findByTestId('option-mode-full_access'))
    const confirm = await screen.findByTestId('danger-confirm')
    expect(confirm).toHaveTextContent('Full access')
    expect(confirm).toHaveTextContent('No sandbox, no approvals')
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ mode: 'read_only' })) // 还没确认
    await user.click(screen.getByTestId('danger-confirm-submit'))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ mode: 'full_access' }))
    expect(screen.getByTestId('option-mode')).toHaveAttribute('data-risk', 'dangerous')
  })

  it('disables full_access when the node forbids dangerous modes', async () => {
    const user = userEvent.setup()
    const agent = codexAgent()
    render(<SettingsControls agent={agent} value={resolveDefaults(agent, [])} onChange={() => {}} dangerousForbidden customKey="n:codex" idPrefix="option" />)
    await user.click(screen.getByTestId('option-mode'))
    expect(await screen.findByTestId('option-mode-full_access')).toHaveAttribute('aria-disabled', 'true')
    expect(screen.getByTestId('option-mode-auto_review')).not.toHaveAttribute('aria-disabled', 'true')
  })

  it('renders the bool and select options in More, and reports their values', async () => {
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Controls onChange={onChange} />)
    await user.click(screen.getByTestId('option-more'))
    expect(await screen.findByTestId('option-extra-fast_mode')).toHaveTextContent('Faster responses, higher usage')
    await user.click(screen.getByRole('switch', { name: 'Fast mode' }))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ options: expect.objectContaining({ fast_mode: true, plan_mode: false }) }))
    await user.click(screen.getByRole('switch', { name: 'Plan mode' }))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ options: expect.objectContaining({ fast_mode: true, plan_mode: true }) }))

    const summarySelect = screen.getByLabelText('Reasoning summary')
    expect(within(summarySelect).getAllByRole('option').map((o) => o.textContent)).toEqual(['auto', 'concise', 'detailed', 'none'])
    await user.selectOptions(summarySelect, 'detailed')
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ options: expect.objectContaining({ model_reasoning_summary: 'detailed' }) }))
  })

  it('works in the stacked layout of the session info panel (different test-id prefix)', async () => {
    const user = userEvent.setup()
    render(<Controls layout="stacked" idPrefix="info" />)
    expect(screen.getByTestId('info-model')).toHaveTextContent('Codex Large')
    expect(screen.getByTestId('info-thinking')).toHaveAttribute('data-value', 'medium')
    await openThinking(user, 'info')
    expect(screen.getByTestId('info-thinking-medium')).toHaveAttribute('aria-checked', 'true')
    await user.keyboard('{Escape}')
    expect(screen.getByTestId('info-mode')).toHaveAttribute('data-value', 'workspace')
    await user.click(screen.getByTestId('info-more'))
    expect(await screen.findByTestId('option-extra-plan_mode')).toBeInTheDocument()
  })

  it('copes with a Codex whose model list could not be fetched (no models, custom ids allowed): no blank label, no empty model sent', async () => {
    const agent = codexAgent({ models: [], allow_custom_model: true })
    const sel = resolveDefaults(agent, [])
    expect(sel.model).toBe('')
    expect(toSessionSettings(sel)).not.toHaveProperty('model')
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Controls agent={agent} onChange={onChange} />)
    expect(screen.getByTestId('option-model')).toHaveTextContent('Agent default')
    expect(screen.queryByTestId('option-thinking')).toBeNull()
    await user.click(screen.getByTestId('option-model'))
    await user.click(await screen.findByTestId('option-model-custom'))
    await user.type(await screen.findByTestId('option-model-custom-input'), 'my-codex-model')
    await user.click(screen.getByRole('button', { name: 'Use' }))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ model: 'my-codex-model' }))
    expect(screen.getByTestId('option-model')).toHaveTextContent('my-codex-model')
  })
})

describe('Codex: new-session form', () => {
  function mountForm(agents = [codexAgent()], defaults?: Parameters<typeof nodeEntry>[0]['agentDefaults']) {
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents, agentDefaults: defaults ?? {} }) }, order: [NODE], loaded: true })
    return render(<NewSessionForm nodeId={NODE} project="app" />)
  }

  it('creates a session with Codex’s defaults: agent id, model, thinking, the normal default mode and all option defaults', async () => {
    rpc.handlers['session.create'] = () => summary({ id: 's1', agent: 'codex' })
    const user = userEvent.setup()
    mountForm()
    expect(screen.queryByTestId('new-session-agent')).toBeNull() // 只有一个可用 agent
    expect(screen.getByTestId('option-mode')).toHaveAttribute('data-value', 'workspace')
    await user.type(screen.getByTestId('composer-input'), 'fix the bug')
    await user.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(rpc.methods()).toContain('session.create'))
    const call = rpc.calls.find((c) => c.method === 'session.create')!
    expect(call.params).toMatchObject({
      project: 'app',
      agent: 'codex',
      prompt: 'fix the bug',
      settings: { model: 'codex-large', thinking: 'medium', mode: 'workspace', options: { fast_mode: false, plan_mode: false, model_reasoning_summary: 'auto' } },
    })
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/s/$session', params: { node: 'nnnnnnnn', session: 's1' } }))
  })

  it('lets the user pick Codex among several agents and then shows Codex’s controls', async () => {
    const user = userEvent.setup()
    mountForm([mockAgent(), codexAgent()])
    // agent 选择器是下拉：触发器上带当前 agent 的 id，选项在打开之后才有
    expect(screen.getByTestId('new-session-agent')).toHaveAttribute('data-value', 'mock')
    await user.click(screen.getByTestId('new-session-agent'))
    expect(await screen.findByTestId('new-session-agent-mock')).toHaveAttribute('aria-checked', 'true')
    await user.click(screen.getByTestId('new-session-agent-codex'))
    expect(screen.getByTestId('new-session-agent')).toHaveAttribute('data-value', 'codex')
    expect(screen.getByTestId('option-model')).toHaveTextContent('Codex Large')
    expect(screen.getByTestId('option-mode')).toHaveTextContent('Workspace')
    await openThinking(user)
    expect(screen.getByTestId('option-thinking-xhigh')).toBeInTheDocument()
  })

  it('never preselects full_access, even when the node default points at it, and says why', () => {
    mountForm([codexAgent()], { codex: { model: 'codex-large', thinking: 'high', mode: 'full_access', options: {} } })
    expect(screen.getByTestId('option-mode')).toHaveAttribute('data-value', 'workspace')
    expect(screen.getByTestId('option-thinking')).toHaveAttribute('data-value', 'high') // 其余默认照用
    expect(screen.getByTestId('dangerous-default-hint')).toHaveTextContent('Full access')
  })

  it('shows the sign-in card with the `codex login` command, and no composer, when Codex is the only agent and is not signed in', async () => {
    mountForm([codexNotAuthenticated()])
    const card = await screen.findByTestId('agent-issue')
    expect(card).toHaveTextContent('Codex')
    expect(card).toHaveTextContent('Codex is not signed in.')
    expect(within(card).getByText('codex login').tagName).toBe('CODE')
    expect(screen.queryByTestId('composer-input')).toBeNull()
  })
})

describe('Codex: issue card and node settings panel', () => {
  it('AgentIssueCard names the problem and offers the hint as a copyable command', () => {
    render(<AgentIssueCard agent={codexNotAuthenticated()} />)
    const card = screen.getByTestId('agent-issue')
    expect(card).toHaveTextContent('Codex — Not signed in')
    expect(within(card).getByText('codex login')).toBeInTheDocument()
    expect(within(card).getByRole('button', { name: 'Copy' })).toBeInTheDocument()
  })

  it('NoAgentsNotice lists a not-signed-in Codex next to a not-installed Claude Code', () => {
    const missing = mockAgent({ id: 'claude-code', label: 'Claude Code', driver: 'claude-stream-json', status: 'not_installed', issue: { code: 'not_installed', message: 'claude was not found', hint: 'npm i -g @anthropic-ai/claude-code' } })
    render(<NoAgentsNotice agents={[missing, codexNotAuthenticated()]} />)
    const cards = screen.getAllByTestId('agent-issue')
    expect(cards).toHaveLength(2)
    expect(cards[1]).toHaveTextContent('codex login')
  })

  it('AgentTab: a signed-in Codex card shows status, version, driver and editable defaults; saving sends them', async () => {
    rpc.handlers['node.set_agent_defaults'] = () => ({})
    rpc.handlers['agent.list'] = () => ({ agents: [codexAgent()], etag: 'e2', not_modified: true })
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [codexAgent()] }) }, order: [NODE], loaded: true })
    const user = userEvent.setup()
    render(<AgentTab nodeId={NODE} />)
    const card = screen.getByTestId('agent-card')
    expect(within(card).getByTestId('agent-status')).toHaveTextContent('Ready')
    expect(card).toHaveTextContent('v0.157.1')
    expect(card).toHaveTextContent('codex-app-server')
    expect(card).toHaveTextContent('/usr/local/bin/codex')
    expect(screen.queryByTestId('agent-issue')).toBeNull()
    expect(screen.getByTestId('default-codex-mode')).toHaveAttribute('data-value', 'workspace')
    expect(screen.getByTestId('agent-defaults-save')).toBeDisabled()

    await openThinking(user, 'default-codex')
    await user.click(screen.getByTestId('default-codex-thinking-high'))
    expect(screen.getByTestId('default-codex-thinking')).toHaveAttribute('data-value', 'high')
    await user.click(screen.getByTestId('default-codex-mode'))
    await user.click(await screen.findByTestId('default-codex-mode-auto_review'))
    expect(screen.getByTestId('agent-defaults-save')).toBeEnabled()
    await user.click(screen.getByTestId('agent-defaults-save'))
    await waitFor(() => expect(rpc.methods()).toContain('node.set_agent_defaults'))
    expect(rpc.calls.find((c) => c.method === 'node.set_agent_defaults')?.params).toEqual({
      agent: 'codex',
      model: 'codex-large',
      thinking: 'high',
      mode: 'auto_review',
      options: { fast_mode: false, plan_mode: false, model_reasoning_summary: 'auto' },
    })
  })

  it('AgentTab: a not-signed-in Codex card shows the issue and `codex login` instead of the controls', () => {
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [codexNotAuthenticated()] }) }, order: [NODE], loaded: true })
    render(<AgentTab nodeId={NODE} />)
    const card = screen.getByTestId('agent-card')
    expect(card.querySelector('[data-agent="codex"]')).toHaveAttribute('data-status', 'not_authenticated')
    expect(within(card).getByTestId('agent-status')).toHaveTextContent('Not signed in')
    expect(within(card).getByTestId('agent-issue')).toHaveTextContent('codex login')
    expect(screen.queryByTestId('default-codex-model')).toBeNull()
    expect(screen.queryByTestId('agent-defaults-save')).toBeNull()
  })
})

// ---- 事件 ----

function listProps(over: Partial<MessageListProps> = {}): MessageListProps {
  return {
    blocks: [],
    pending: [],
    hasMoreOlder: false,
    loadingOlder: false,
    onLoadOlder: () => {},
    deviceLabel: (id) => id,
    respondingIds: new Set(),
    onRespond: () => {},
    onRetryPending: () => {},
    onEditPending: () => {},
    onDeletePending: () => {},
    modelLabel: (id) => id,
    modeLabel: (id) => id,
    ...over,
  }
}

const shellCall = (id: string, command: string) =>
  ({ type: 'tool_call', id, name: 'commandExecution', category: 'shell', title: command, input: { command: ['bash', '-lc', command], cwd: '/work/app' }, detail: { command, cwd: '/work/app' } }) as const

describe('Codex: event rendering', () => {
  it('streams command output into a running shell block, then replaces it with the final result and exit code', async () => {
    const start = evs([{ type: 'turn_start', turn_id: 1, message_id: 'm1' }, shellCall('c1', 'cargo test')])
    const { rerender } = render(<MessageList {...listProps({ blocks: buildView(start).blocks })} />)
    // 刚开始，还没有任何输出：在跑，且不能说「没有输出」
    let tool = screen.getByTestId('tool-block')
    expect(tool).toHaveAttribute('data-state', 'running')
    expect(tool).toHaveTextContent('$ cargo test')
    expect(screen.queryByText('No output')).toBeNull()
    await userEvent.click(within(tool).getByRole('button'))

    const streaming = [...start, ...evs([{ type: 'tool_output_delta', id: 'c1', text: 'running 3 tests\n' }, { type: 'tool_output_delta', id: 'c1', text: 'test a ... ok\n' }])]
    rerender(<MessageList {...listProps({ blocks: buildView(streaming).blocks })} />)
    tool = screen.getByTestId('tool-block')
    expect(tool).toHaveAttribute('data-state', 'running')
    expect(screen.getByTestId('tool-output')).toHaveTextContent('running 3 tests')
    expect(screen.getByTestId('tool-output')).toHaveTextContent('test a ... ok')

    const done = [...streaming, ...evs([{ type: 'tool_result', id: 'c1', output: 'running 3 tests\ntest a ... ok\n', is_error: false, detail: { exit_code: 0 } }])]
    rerender(<MessageList {...listProps({ blocks: buildView(done).blocks })} />)
    expect(screen.getByTestId('tool-block')).toHaveAttribute('data-state', 'done')
    expect(screen.getAllByTestId('tool-block')).toHaveLength(1) // delta 挂回了同一个块
    expect(screen.getByTestId('tool-output')).toHaveTextContent('test a ... ok')
  })

  it('keeps the streamed text when the final result carries no output, and flags a non-zero exit', async () => {
    const view = buildView(
      evs([
        shellCall('c1', 'make'),
        { type: 'tool_output_delta', id: 'c1', text: 'error: nothing to do\n' },
        { type: 'tool_result', id: 'c1', output: '', is_error: true, detail: { exit_code: 2, live_output_truncated: true } },
      ]),
    )
    render(<MessageList {...listProps({ blocks: view.blocks })} />)
    expect(screen.getByTestId('tool-block')).toHaveAttribute('data-state', 'error')
    await userEvent.click(within(screen.getByTestId('tool-block')).getByRole('button'))
    expect(screen.getByTestId('tool-output')).toHaveTextContent('error: nothing to do')
    expect(screen.getByTestId('tool-block')).toHaveTextContent('exit 2')
    expect(screen.getByTestId('tool-live-truncated')).toBeInTheDocument()
  })

  it('shows a declined tool call (status: declined → is_error) as an error block without output', async () => {
    const view = buildView(evs([shellCall('c1', 'rm -rf build'), { type: 'tool_result', id: 'c1', output: '', is_error: true }]))
    render(<MessageList {...listProps({ blocks: view.blocks })} />)
    expect(screen.getByTestId('tool-block')).toHaveAttribute('data-state', 'error')
    expect(screen.getByTestId('tool-block')).toHaveTextContent('$ rm -rf build')
    await userEvent.click(within(screen.getByTestId('tool-block')).getByRole('button'))
    expect(screen.getByText('No output')).toBeInTheDocument() // 已经结束了，这时才说没有输出
  })

  it('renders fileChange as a file_edit block with its diff, and keeps the changed-file list', () => {
    const diff = ['--- a/src/lib.rs', '+++ b/src/lib.rs', '@@ -1,1 +1,2 @@', ' fn main() {}', '+fn extra() {}'].join('\n')
    const view = buildView(
      evs([
        { type: 'tool_call', id: 'f1', name: 'fileChange', category: 'file_edit', title: 'src/lib.rs', input: { changes: [{ path: 'src/lib.rs', kind: 'update' }] }, detail: { path: 'src/lib.rs', edit_kind: 'edit' } },
        { type: 'file_changed', path: 'src/lib.rs', kind: 'modified' },
        { type: 'tool_result', id: 'f1', output: 'ok', is_error: false, detail: { unified_diff: diff } },
      ]),
    )
    render(<MessageList {...listProps({ blocks: view.blocks })} />)
    const tool = screen.getByTestId('tool-block')
    expect(tool).toHaveAttribute('data-category', 'file_edit')
    expect(tool).toHaveTextContent('src/lib.rs')
    expect(tool).toHaveTextContent('+1')
    expect(view.changedFiles).toEqual({ 'src/lib.rs': 'modified' })
  })

  it('renders mcp and subagent calls with a generic block, and a webSearch as a search block', () => {
    const view = buildView(
      evs([
        { type: 'tool_call', id: 'm1', name: 'mcpToolCall', category: 'mcp', title: 'github.search_issues', input: { server: 'github', tool: 'search_issues' }, detail: { server: 'github', tool: 'search_issues' } },
        { type: 'tool_call', id: 'a1', name: 'collabAgentToolCall', category: 'subagent', title: 'review the diff', input: {}, detail: { description: 'review the diff' } },
        { type: 'tool_call', id: 'w1', name: 'webSearch', category: 'search', title: 'rust tokio join', input: { query: 'rust tokio join' }, detail: { query: 'rust tokio join' } },
      ]),
    )
    render(<MessageList {...listProps({ blocks: view.blocks })} />)
    const blocks = screen.getAllByTestId('tool-block')
    expect(blocks.map((b) => b.getAttribute('data-category'))).toEqual(['mcp', 'subagent', 'search'])
    expect(blocks[0]).toHaveTextContent('github.search_issues') // 来自 title，不是 item 类型 mcpToolCall
    expect(blocks[0]).not.toHaveTextContent('mcpToolCall')
    expect(blocks[1]).toHaveTextContent('review the diff')
    expect(blocks[1]).not.toHaveTextContent('collabAgentToolCall')
    expect(blocks[2]).toHaveTextContent('Search “rust tokio join”')
  })

  it('updates one plan card in place as turn/plan/updated repeats, with all three statuses', () => {
    const e1 = ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' })
    const p1 = ev({ type: 'plan', entries: [{ content: 'Read the code', status: 'in_progress' }, { content: 'Write the fix', status: 'pending' }, { content: 'Run tests', status: 'pending' }] })
    const view1 = buildView([e1, p1])
    const { rerender } = render(<MessageList {...listProps({ blocks: view1.blocks })} />)
    expect(screen.getByTestId('plan-card')).toHaveTextContent('0/3')

    const p2 = ev({ type: 'plan', entries: [{ content: 'Read the code', status: 'completed' }, { content: 'Write the fix', status: 'in_progress' }, { content: 'Run tests', status: 'pending' }] })
    const view2 = buildView([e1, p1, ev({ type: 'text_delta', text: 'Working on it' }), p2])
    rerender(<MessageList {...listProps({ blocks: view2.blocks })} />)
    expect(screen.getAllByTestId('plan-card')).toHaveLength(1)
    const card = screen.getByTestId('plan-card')
    expect(card).toHaveTextContent('1/3')
    const rows = within(card).getAllByRole('listitem')
    expect(rows.map((r) => r.getAttribute('data-status'))).toEqual(['completed', 'in_progress', 'pending'])
    // 新一轮的计划是另一张卡
    const view3 = buildView([e1, p1, p2, ev({ type: 'turn_done', turn_id: 1, reason: 'completed' }), ev({ type: 'turn_start', turn_id: 2, message_id: 'm2' }), ev({ type: 'plan', entries: [{ content: 'Next', status: 'pending' }] })])
    rerender(<MessageList {...listProps({ blocks: view3.blocks })} />)
    expect(screen.getAllByTestId('plan-card')).toHaveLength(2)
  })

  it('shows Codex usage (no cost) and an interrupted turn as cancelled', () => {
    const view = buildView(
      evs([
        { type: 'turn_start', turn_id: 1, message_id: 'm1' },
        { type: 'thinking_delta', text: '' }, // 只有「正在思考」没有内容
        { type: 'thinking_done', duration_ms: 2000 },
        { type: 'turn_done', turn_id: 1, reason: 'cancelled', usage: { input_tokens: 4200, output_tokens: 310, cache_read_tokens: 3000, duration_ms: 9000, model: 'codex-large' } },
      ]),
    )
    render(<MessageList {...listProps({ blocks: view.blocks })} />)
    const usage = screen.getByTestId('turn-usage')
    expect(usage).toHaveTextContent('codex-large')
    expect(usage).toHaveTextContent('4.2k in')
    expect(usage).toHaveTextContent('Stopped')
    expect(usage.textContent).not.toContain('$')
    expect(screen.getByTestId('thinking-block')).toHaveTextContent('Thought for 2 s')
  })

  it('renders a Codex approval request: command, cwd, the reason, and the three options by id', async () => {
    const onRespond = vi.fn()
    const user = userEvent.setup()
    render(
      <PermissionCard
        deviceLabel={(d) => d}
        onRespond={onRespond}
        block={{
          type: 'permission',
          key: 'perm-r1',
          seq: 1,
          ts: 1,
          requestId: 'r1',
          kind: 'tool',
          title: 'Run command',
          detail: { tool: { tool: 'commandExecution', category: 'shell', cwd: '/work/app', input: { command: 'npm install' }, detail: { command: 'npm install', description: 'Needs network access to download packages' } } },
          options: [
            { id: 'accept', label: 'Allow', kind: 'allow_once' },
            { id: 'acceptForSession', label: 'Allow for this session', kind: 'allow_always' },
            { id: 'decline', label: 'Deny', kind: 'reject_once' },
          ],
        }}
      />,
    )
    expect(screen.getByTestId('permission-command')).toHaveTextContent('$ npm install')
    expect(screen.getByTestId('permission-reason')).toHaveTextContent('Needs network access')
    expect(screen.getByText('in /work/app')).toBeInTheDocument()
    await user.click(screen.getByTestId('permission-option-acceptForSession'))
    expect(onRespond).toHaveBeenCalledWith({ request_id: 'r1', option_id: 'acceptForSession', message: undefined })
  })
})

// ---- Codex 的 error code ----

const errorEv = (code: string, message: string, recoverable = true) => ev({ type: 'error', code, message, recoverable })

describe('Codex: error events', () => {
  it('shows agent_reconnecting as a muted one-line notice — not a red error block, not an alert, and never with a retry button', () => {
    const view = buildView([ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' }), errorEv('agent_reconnecting', 'Reconnecting... 2/5')])
    render(<MessageList {...listProps({ blocks: view.blocks, onRetryTurn: () => {} })} />)
    const notice = screen.getByTestId('error-notice')
    expect(notice).toHaveAttribute('data-code', 'agent_reconnecting')
    expect(notice).toHaveAttribute('role', 'status')
    expect(notice).toHaveTextContent('Connection to the model dropped — reconnecting…')
    expect(notice).toHaveTextContent('Reconnecting... 2/5') // adapter 的原文跟在后面
    expect(notice.className).toContain('text-fg-subtle')
    expect(notice.className).not.toContain('danger')
    expect(screen.queryByTestId('error-block')).toBeNull()
    expect(screen.queryByRole('alert')).toBeNull()
    expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull()
  })

  it('shows agent_context_reset as a muted notice like a reconnect: the agent can no longer see the earlier conversation, no red block, no retry', () => {
    const view = buildView([ev({ type: 'turn_start', turn_id: 3, message_id: 'm3' }), errorEv('agent_context_reset', 'Codex could not resume the previous thread (no rollout found); started a new one.')])
    render(<MessageList {...listProps({ blocks: view.blocks, onRetryTurn: () => {} })} />)
    const notice = screen.getByTestId('error-notice')
    expect(notice).toHaveAttribute('data-code', 'agent_context_reset')
    expect(notice).toHaveAttribute('role', 'status')
    expect(notice).toHaveTextContent('Earlier messages in this conversation are no longer visible to the agent')
    expect(notice).toHaveTextContent('no rollout found') // adapter 的原文跟在后面（截断，title 里是全文）
    expect(notice.className).toContain('text-fg-subtle')
    expect(screen.queryByTestId('error-block')).toBeNull()
    expect(screen.queryByRole('alert')).toBeNull()
    expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull()
  })

  it('lets the long context-reset text wrap on a narrow screen instead of overflowing, and keeps the adapter text on one truncated line', () => {
    const view = buildView([errorEv('agent_context_reset', 'x'.repeat(300))])
    render(<MessageList {...listProps({ blocks: view.blocks })} />)
    const notice = screen.getByTestId('error-notice')
    expect(notice.className).toContain('flex-wrap')
    expect(within(notice).getByText('x'.repeat(300)).className).toContain('truncate')
  })

  it('treats agent_context_reset as transparent for Retry too, and has Chinese copy', async () => {
    const blocks = buildView([errorEv('rate_limited', 'x'), errorEv('agent_context_reset', 'r')]).blocks
    expect(retryableErrorKey(blocks)).toBe(blocks[0]!.key)
    await i18n.changeLanguage('zh')
    try {
      render(<MessageList {...listProps({ blocks: buildView([errorEv('agent_context_reset', 'r')]).blocks })} />)
      expect(screen.getByTestId('error-notice')).toHaveTextContent('这段对话之前的内容 agent 已经看不到了')
    } finally {
      await i18n.changeLanguage('en')
    }
  })

  it('does not take a code like “constructor” or “__proto__” for a known one', () => {
    const view = buildView([errorEv('constructor', 'weird'), errorEv('__proto__', 'weirder')])
    render(<MessageList {...listProps({ blocks: view.blocks })} />)
    const blocks = screen.getAllByTestId('error-block')
    expect(blocks.map((b) => b.getAttribute('data-severity'))).toEqual(['error', 'error'])
    expect(screen.queryByTestId('error-explained')).toBeNull()
  })

  it('renders every one of up to five reconnect notices as its own single line', () => {
    const blocks = buildView([ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' }), ...[1, 2, 3, 4, 5].map((n) => errorEv('agent_reconnecting', `Reconnecting... ${n}/5`))]).blocks
    render(<MessageList {...listProps({ blocks })} />)
    const notices = screen.getAllByTestId('error-notice')
    expect(notices).toHaveLength(5)
    // 一行：消息部分截断，不换行
    expect(within(notices[0]!).getByText('Reconnecting... 1/5').className).toContain('truncate')
  })

  it('shows unsupported_request as a yellow warning (the request was declined), without a retry button', () => {
    const view = buildView([errorEv('unsupported_request', 'Declined mcpServer/elicitation/request: not supported by anycon yet')])
    render(<MessageList {...listProps({ blocks: view.blocks, onRetryTurn: () => {} })} />)
    const block = screen.getByTestId('error-block')
    expect(block).toHaveAttribute('data-severity', 'warning')
    expect(block).toHaveAttribute('role', 'status')
    expect(block.className).toContain('border-warning')
    expect(block).toHaveTextContent('Declined mcpServer/elicitation/request')
    expect(screen.getByTestId('error-explained')).toHaveTextContent('so the request was declined')
    expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull()
  })

  it.each([
    ['context_window_exceeded', 'Codex ran out of room in the model’s context window.', /no longer fits in the model’s context window/],
    ['sandbox_error', 'The sandbox failed to run the command', /Check the session’s permission mode/],
    ['policy_violation', 'The request was blocked by a usage policy', /usage policy stopped this request/],
    ['rate_limited', 'Usage limit reached', /rate or usage limit/],
    ['auth_failed', 'Not logged in', /not signed in, or its credentials were rejected/],
  ])('shows %s as a red error with the adapter’s message, a short explanation and the code', (code, message, explanation) => {
    const view = buildView([errorEv(code, message, code !== 'auth_failed')])
    render(<MessageList {...listProps({ blocks: view.blocks })} />)
    const block = screen.getByTestId('error-block')
    expect(block).toHaveAttribute('data-severity', 'error')
    expect(block).toHaveAttribute('role', 'alert')
    expect(block.className).toContain('border-danger')
    expect(block).toHaveTextContent(message)
    expect(screen.getByTestId('error-explained')).toHaveTextContent(explanation)
    expect(block).toHaveTextContent(code)
  })

  it('offers “Retry” only on the last recoverable real error, and notices do not take it away', async () => {
    const onRetry = vi.fn()
    const user = userEvent.setup()
    const blocks = buildView([errorEv('context_window_exceeded', 'too long'), errorEv('agent_reconnecting', 'Reconnecting... 1/5'), errorEv('unsupported_request', 'declined')]).blocks
    render(<MessageList {...listProps({ blocks, onRetryTurn: onRetry })} />)
    await user.click(screen.getByRole('button', { name: 'Retry' }))
    expect(onRetry).toHaveBeenCalledTimes(1)
  })

  it('retryableErrorKey skips notices and warnings', () => {
    const blocks = (...evts: ReturnType<typeof errorEv>[]) => buildView(evts).blocks
    const withReal = blocks(errorEv('rate_limited', 'x'), errorEv('agent_reconnecting', 'r'))
    expect(retryableErrorKey(withReal)).toBe(withReal[0]!.key)
    expect(retryableErrorKey(blocks(errorEv('agent_reconnecting', 'r'), errorEv('unsupported_request', 'u')))).toBeNull()
    expect(retryableErrorKey(blocks(errorEv('auth_failed', 'x', false)))).toBeNull() // 不可恢复
  })

  it('leaves an error code it has no copy for as it was: red, message and code only', () => {
    const view = buildView([errorEv('agent_exited', 'The agent process exited')])
    render(<MessageList {...listProps({ blocks: view.blocks })} />)
    expect(screen.getByTestId('error-block')).toHaveAttribute('data-severity', 'error')
    expect(screen.queryByTestId('error-explained')).toBeNull()
  })

  it('has Chinese copy for all of them', async () => {
    await i18n.changeLanguage('zh')
    try {
      const view = buildView([errorEv('agent_reconnecting', 'Reconnecting... 3/5'), errorEv('unsupported_request', 'declined'), errorEv('sandbox_error', 'failed')])
      render(<MessageList {...listProps({ blocks: view.blocks })} />)
      expect(screen.getByTestId('error-notice')).toHaveTextContent('与模型的连接中断，正在重连…')
      const [warn, err] = screen.getAllByTestId('error-explained')
      expect(warn).toHaveTextContent('已自动拒绝')
      expect(err).toHaveTextContent('沙箱')
    } finally {
      await i18n.changeLanguage('en')
    }
  })
})

// ---- Codex 的 tool_call：只认 category + detail，不认工具名 ----

const RAW_NAMES = ['commandExecution', 'fileChange', 'mcpToolCall', 'collabAgentToolCall', 'imageView', 'mcp__github__search_issues', 'spawnAgent', '/bin/zsh']

describe('Codex: tool_call shapes (titles and icons come from category and detail, not the item type)', () => {
  const adapterDiff = ['--- /dev/null', '+++ b/new.txt', '@@ -0,0 +1,1 @@', '+hello', '--- a/src/lib.rs', '+++ b/src/lib.rs', '@@ -1 +1 @@', '-fn a() {}', '+fn b() {}', ''].join('\n')
  const calls = [
    // commandExecution：标题 / detail.command 是拆掉 `/bin/zsh -lc '…'` 之后的命令，input.command 保留原样
    { type: 'tool_call', id: 'c1', name: 'commandExecution', category: 'shell', title: 'cargo test -p core', input: { command: "/bin/zsh -lc 'cargo test -p core'", cwd: '/work/app' }, detail: { command: 'cargo test -p core', cwd: '/work/app' } },
    { type: 'tool_result', id: 'c1', output: 'ok', is_error: false, detail: { exit_code: 0 } },
    // fileChange 一个文件
    { type: 'tool_call', id: 'f1', name: 'fileChange', category: 'file_edit', title: 'Edit src/lib.rs', input: { changes: [{ path: '/work/app/src/lib.rs', kind: 'update', diff: '@@ -1 +1 @@\n-a\n+b\n' }] }, detail: { path: '/work/app/src/lib.rs', edit_kind: 'edit' } },
    // fileChange 两个文件：detail.path 只是第一个，标题要显示「Edit 2 files」
    { type: 'tool_call', id: 'f2', name: 'fileChange', category: 'file_edit', title: 'Edit 2 files', input: { changes: [{ path: '/work/app/new.txt', kind: 'add', diff: 'hello\n' }, { path: '/work/app/src/lib.rs', kind: 'update', diff: '@@ -1 +1 @@\n-fn a() {}\n+fn b() {}\n' }] }, detail: { path: '/work/app/new.txt', edit_kind: 'write' } },
    { type: 'tool_result', id: 'f2', output: 'ok', is_error: false, detail: { unified_diff: adapterDiff } },
    // mcpToolCall
    { type: 'tool_call', id: 'm1', name: 'mcp__github__search_issues', category: 'mcp', title: 'github/search_issues', input: { q: 'bug' }, detail: { server: 'github', tool: 'search_issues' } },
    // collabAgentToolCall
    { type: 'tool_call', id: 'a1', name: 'spawnAgent', category: 'subagent', title: 'Explore the repo', input: { tool: 'spawnAgent', prompt: 'Explore the repo' }, detail: { description: 'Explore the repo' } },
    // webSearch / imageView / dynamicToolCall
    { type: 'tool_call', id: 'w1', name: 'webSearch', category: 'search', title: 'Search: tokio join', input: { query: 'tokio join' }, detail: { query: 'tokio join' } },
    { type: 'tool_call', id: 'i1', name: 'imageView', category: 'file_read', title: 'View shot.png', input: { path: '/work/app/shot.png' }, detail: { path: '/work/app/shot.png' } },
    { type: 'tool_call', id: 'd1', name: 'search.docs', category: 'other', title: 'search.docs', input: { q: 'x' } },
  ] as const

  function blocks() {
    return buildView(evs(calls.map((c) => ({ ...c }) as never))).blocks
  }

  it('titles each block from detail / title and never shows the item type or the shell wrapper', () => {
    render(<MessageList {...listProps({ blocks: blocks() })} />)
    const tools = screen.getAllByTestId('tool-block')
    expect(tools.map((t) => t.getAttribute('data-category'))).toEqual(['shell', 'file_edit', 'file_edit', 'mcp', 'subagent', 'search', 'file_read', 'other'])
    const button = (i: number): string => within(tools[i]!).getAllByRole('button')[0]!.textContent ?? ''
    expect(button(0)).toContain('$ cargo test -p core')
    expect(button(1)).toContain('/work/app/src/lib.rs') // 单文件：路径
    expect(button(2)).toContain('Edit 2 files') // 多文件：标题，不是第一个文件的路径
    expect(button(2)).not.toContain('/work/app/new.txt')
    expect(button(3)).toContain('github/search_issues')
    expect(button(4)).toContain('Explore the repo')
    expect(button(5)).toContain('Search “tokio join”')
    expect(button(6)).toContain('Read /work/app/shot.png')
    expect(button(7)).toContain('search.docs')
    for (const t of tools) for (const raw of RAW_NAMES) expect(t.textContent).not.toContain(raw)
  })

  it('renders by category: a Codex item type and a Claude Code tool name of the same category look the same, and an unknown name still follows its category (no per-tool icon)', () => {
    const mk = (name: string, category: 'shell' | 'file_edit', extra: object = {}) => ({ type: 'tool_call' as const, id: name, name, category, title: 'x', input: {}, ...extra })
    const { container } = render(
      <MessageList
        {...listProps({
          blocks: buildView(evs([mk('Bash', 'shell'), mk('commandExecution', 'shell'), mk('some-future-shell-tool', 'shell'), mk('Edit', 'file_edit'), mk('fileChange', 'file_edit')])).blocks,
        })}
      />,
    )
    const rows = [...container.querySelectorAll('[data-testid="tool-block"]')]
    expect(rows.map((b) => b.getAttribute('data-category'))).toEqual(['shell', 'shell', 'shell', 'file_edit', 'file_edit'])
    // 收起时是一行灰字：前面不再有图标（只剩末尾的展开箭头 / 运行中转圈）
    for (const r of rows) expect(r.querySelector('button > svg:first-child')).toBeNull()
  })

  it('shows a multi-file fileChange as one diff section per file, named by path', async () => {
    const user = userEvent.setup()
    render(<MessageList {...listProps({ blocks: blocks() })} />)
    const edit = screen.getAllByTestId('tool-block')[2]!
    await user.click(within(edit).getAllByRole('button')[0]!)
    const files = await within(edit).findAllByTestId('diff-file')
    expect(files.map((f) => f.getAttribute('data-path'))).toEqual(['new.txt', 'src/lib.rs'])
    expect(files[0]).toHaveTextContent('new.txt')
  })

  it('DiffView takes the adapter’s header-less diffs for a single file too, without a filename bar', async () => {
    render(<DiffView diff={['--- a/src/lib.rs', '+++ b/src/lib.rs', '@@ -1 +1 @@', '-a', '+b', ''].join('\n')} />)
    expect(screen.getAllByTestId('diff-file')).toHaveLength(1)
    expect(screen.queryByText('src/lib.rs')).toBeNull()
  })

  it('names a multi-file patch approval by all its paths and previews the diff before it is applied', async () => {
    const user = userEvent.setup()
    render(
      <PermissionCard
        deviceLabel={(d) => d}
        onRespond={() => {}}
        block={{
          type: 'permission',
          key: 'perm-r2',
          seq: 1,
          ts: 1,
          requestId: 'r2',
          kind: 'tool',
          title: 'Apply patch',
          detail: { tool: { tool: 'fileChange', category: 'file_edit', input: calls[3].input, detail: { path: '/work/app/new.txt', edit_kind: 'write', description: 'Codex wants to write outside the workspace' } } },
          options: [{ id: 'accept', label: 'Allow', kind: 'allow_once' }],
        }}
      />,
    )
    expect(screen.getByTestId('permission-command')).toHaveTextContent('/work/app/new.txt')
    expect(screen.getByTestId('permission-command')).toHaveTextContent('/work/app/src/lib.rs')
    expect(screen.getByTestId('permission-reason')).toHaveTextContent('outside the workspace')
    await user.click(screen.getByRole('button', { name: /Details/ }))
    expect(await screen.findAllByTestId('diff-file')).toHaveLength(2)
  })

  it('exports Codex tool calls with a category heading and the command, not the item type', () => {
    const md = exportMarkdown('Codex session', blocks())
    expect(md).toContain('> **Shell** — `$ cargo test -p core`')
    expect(md).toContain('> **Edit** — `/work/app/src/lib.rs`')
    expect(md).toContain('> **MCP** — `github/search_issues`')
    for (const raw of RAW_NAMES) expect(md).not.toContain(raw)
  })

  describe('patch approvals from the Codex adapter (multi-paragraph description, unknown contents, long titles)', () => {
    const DESCRIPTION = 'Codex wants to write outside the workspace\n\n- edit /etc/hosts\n- create /srv/app/very/long/path/that/keeps/going/and/going/without/any/break/point/file.txt\n\nThis also grants write access under /srv/app for the rest of the session.'
    const options = [
      { id: 'accept', label: 'Allow', kind: 'allow_once' as const },
      { id: 'decline', label: 'Deny', kind: 'reject_once' as const },
    ]
    const patchBlock = (over: { title?: string; input?: unknown; description?: string | null }) => ({
      type: 'permission' as const,
      key: 'perm-p',
      seq: 1,
      ts: 1,
      requestId: 'p',
      kind: 'tool' as const,
      title: over.title ?? 'Edit 2 files (/etc/hosts, /srv/app/a.txt)',
      detail: { tool: { tool: 'fileChange', category: 'file_edit' as const, input: over.input ?? { changes: [{ path: '/etc/hosts', kind: 'update', diff: '@@ -1 +1 @@\n-a\n+b\n' }, { path: '/srv/app/a.txt', kind: 'add', diff: 'x\n' }] }, detail: over.description === null ? { path: '/etc/hosts' } : { path: '/etc/hosts', description: over.description ?? DESCRIPTION } } },
      options,
    })

    it('keeps the line breaks of the multi-paragraph reason and lets long lines wrap', () => {
      render(<PermissionCard deviceLabel={(d) => d} onRespond={() => {}} block={patchBlock({})} />)
      const reason = screen.getByTestId('permission-reason')
      expect(reason.className).toContain('whitespace-pre-wrap')
      expect(reason.className).toContain('[overflow-wrap:anywhere]')
      // 空行和列表行都在（pre-wrap 才会把它们显示出来）
      expect(reason.textContent).toBe(DESCRIPTION)
      expect(reason.textContent).toContain('\n\n- edit /etc/hosts\n- create /srv/app/')
    })

    it('wraps a long multi-file title with absolute paths instead of overflowing, in the pending card', () => {
      const title = 'Edit 3 files (/etc/hosts, /srv/app/very/long/path/that/keeps/going/and/going/file.txt, /var/log/x.log)'
      render(<PermissionCard deviceLabel={(d) => d} onRespond={() => {}} block={patchBlock({ title })} />)
      const heading = screen.getByText(title)
      expect(heading.className).toContain('[overflow-wrap:anywhere]')
    })

    it('shows a plain patch approval (contents known) without the warning', () => {
      render(<PermissionCard deviceLabel={(d) => d} onRespond={() => {}} block={patchBlock({})} />)
      expect(screen.queryByTestId('permission-unknown-content')).toBeNull()
      expect(screen.getByTestId('permission-card')).not.toHaveAttribute('data-unknown-content')
      expect(screen.getByTestId('permission-option-accept').className).not.toContain('bg-danger')
    })

    const unknownBlock = () =>
      patchBlock({
        title: 'Edit files (patch contents unknown) and allow writing under /srv/app for this session',
        input: { reason: 'write outside the workspace', grantRoot: '/srv/app', contentUnknown: true },
        description: 'write outside the workspace\n\nThe patch contents are unknown: this approval request arrived without the matching item, so the files it changes cannot be shown. Review the session before allowing it.\n\nThis also grants write access under /srv/app for the rest of the session.',
      })

    it('warns when the patch contents are unknown: danger-styled card, an alert line, a cautious Allow button, and the description in full', () => {
      render(<PermissionCard deviceLabel={(d) => d} onRespond={() => {}} block={unknownBlock()} />)
      const card = screen.getByTestId('permission-card')
      expect(card).toHaveAttribute('data-unknown-content', 'true')
      expect(card).not.toHaveAttribute('data-danger') // 不是「危险模式」，不要冒充
      expect(card.innerHTML).toContain('border-danger/50')
      const warning = screen.getByTestId('permission-unknown-content')
      expect(warning).toHaveAttribute('role', 'alert')
      expect(warning).toHaveTextContent('The patch contents could not be shown, so you cannot see what it changes. Be careful before approving.')
      expect(screen.getByTestId('permission-option-accept').className).toContain('bg-danger')
      expect(screen.getByTestId('permission-option-decline')).toBeEnabled()
      // 说明（多段）单独显示；没有命令 / 路径可显示，不把说明当成「命令」
      expect(screen.queryByTestId('permission-command')).toBeNull()
      expect(screen.getByTestId('permission-reason').textContent).toContain('The patch contents are unknown')
      expect(screen.getByTestId('permission-reason').textContent).toContain('This also grants write access under /srv/app')
      // 长标题折行
      expect(screen.getByText(/^Edit files \(patch contents unknown\)/).className).toContain('[overflow-wrap:anywhere]')
    })

    it('still lets the user answer an unknown-contents approval either way', async () => {
      const onRespond = vi.fn()
      const user = userEvent.setup()
      render(<PermissionCard deviceLabel={(d) => d} onRespond={onRespond} block={unknownBlock()} />)
      await user.click(screen.getByTestId('permission-option-decline'))
      expect(onRespond).toHaveBeenLastCalledWith({ request_id: 'p', option_id: 'decline', message: undefined })
      await user.click(screen.getByTestId('permission-option-accept'))
      expect(onRespond).toHaveBeenLastCalledWith({ request_id: 'p', option_id: 'accept', message: undefined })
    })

    it('has Chinese copy for the unknown-contents warning', async () => {
      await i18n.changeLanguage('zh')
      try {
        render(<PermissionCard deviceLabel={(d) => d} onRespond={() => {}} block={unknownBlock()} />)
        expect(screen.getByTestId('permission-unknown-content')).toHaveTextContent('无法显示补丁内容，看不到它会改什么。批准前请格外小心。')
      } finally {
        await i18n.changeLanguage('en')
      }
    })

    it('ignores contentUnknown on anything that is not a tool approval, or when it is not exactly true', () => {
      const { rerender } = render(<PermissionCard deviceLabel={(d) => d} onRespond={() => {}} block={patchBlock({ input: { contentUnknown: 'true' } })} />)
      expect(screen.queryByTestId('permission-unknown-content')).toBeNull()
      rerender(<PermissionCard deviceLabel={(d) => d} onRespond={() => {}} block={patchBlock({ input: { contentUnknown: false } })} />)
      expect(screen.queryByTestId('permission-unknown-content')).toBeNull()
    })
  })
})
