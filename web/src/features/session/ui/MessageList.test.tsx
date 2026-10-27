import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { ev, evs, resetSeq } from '@/test/factories'
import type { PendingMessage } from '@/stores/drafts'
import type { Event } from '@/protocol'
import { applyEvents, buildView } from '../reducer'
import { MessageList, retryableErrorKey, VIRTUALIZE_THRESHOLD, type MessageListProps } from './MessageList'

beforeEach(() => resetSeq())

function props(over: Partial<MessageListProps> = {}): MessageListProps {
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

describe('MessageList', () => {
  it('three agents running in parallel each get their own block; their calls are not interleaved in the main stream', async () => {
    const user = userEvent.setup()
    const agent = (id: string, description: string) => ({ type: 'tool_call', id, name: 'Agent', category: 'subagent', title: description, input: { description }, detail: { description } }) as const
    const bash = (id: string, parent_id: string, command: string) => ({ type: 'tool_call', id, name: 'Bash', category: 'shell', title: command, input: { command }, detail: { command }, parent_id }) as const
    const view = buildView(
      evs([
        { type: 'turn_start', turn_id: 1, message_id: 'm1' },
        agent('a2', 'Backend API'),
        agent('a3', 'Status sync'),
        { type: 'tool_result', id: 'a2', output: 'launched', is_error: false, detail: { background: true } },
        { type: 'tool_result', id: 'a3', output: 'launched', is_error: false, detail: { background: true } },
        bash('c1', 'a2', 'sed -n 1,10p a.ts'),
        bash('c2', 'a3', 'grep -n ship b.ts'),
        bash('c3', 'a2', 'cat c.ts'),
        { type: 'tool_result', id: 'c1', output: '', is_error: false },
        { type: 'tool_result', id: 'a3', output: 'Status sync **done**', is_error: false },
      ]),
    )
    render(<MessageList {...props({ blocks: view.blocks })} />)
    // 主流里只有两个子 agent 块，子调用不单独成行
    const blocks = [...screen.getByTestId('message-list').querySelectorAll(':scope > div > div > [data-testid="tool-block"]')] as HTMLElement[]
    expect(blocks.map((b) => b.dataset.category)).toEqual(['subagent', 'subagent'])

    const [backend, status] = blocks as [HTMLElement, HTMLElement]
    expect(backend).toHaveAttribute('data-state', 'background')
    expect(within(backend).getByTestId('subagent-calls')).toHaveTextContent('2 tool calls')
    // 收起且还在跑：只露出最近的一次调用
    expect(within(backend).getByTestId('subagent-latest')).toHaveTextContent('cat c.ts')
    expect(within(backend).queryByText(/sed -n/)).toBeNull()

    expect(status).toHaveAttribute('data-state', 'done')
    expect(within(status).queryByTestId('subagent-latest')).toBeNull()
    await user.click(within(status).getByRole('button', { name: /Status sync/ }))
    expect(within(status).getByTestId('subagent-children')).toHaveTextContent('grep -n ship b.ts')
    expect(within(status).getByTestId('subagent-result')).toHaveTextContent('Status sync done')

    await user.click(within(backend).getByRole('button', { name: /Backend API/ }))
    const kids = within(backend).getAllByTestId('tool-block')
    expect(kids.map((k) => k.textContent)).toEqual([expect.stringContaining('sed -n 1,10p a.ts'), expect.stringContaining('cat c.ts')])
  })
  it('a background command says so and keeps spinning until its final result arrives', () => {
    const cmd = { type: 'tool_call', id: 'b', name: 'Bash', category: 'shell', title: 'cargo test', input: { command: 'cargo test', run_in_background: true }, detail: { command: 'cargo test' } } as const
    const launched = { type: 'tool_result', id: 'b', output: 'Command running in background with ID: b1.', is_error: false, detail: { background: true } } as const
    const { rerender } = render(<MessageList {...props({ blocks: buildView(evs([cmd, launched])).blocks })} />)
    const row = () => screen.getByTestId('tool-block')
    expect(row()).toHaveAttribute('data-state', 'background')
    expect(row()).toHaveTextContent('running in background')
    expect(within(row()).getByLabelText('Running')).toBeInTheDocument()
    const finished = { type: 'tool_result', id: 'b', output: 'Background command "cargo test" completed (exit code 101)', is_error: true, detail: { exit_code: 101 } } as const
    rerender(<MessageList {...props({ blocks: buildView(evs([cmd, launched, finished])).blocks })} />)
    expect(row()).toHaveAttribute('data-state', 'error')
    expect(row()).toHaveTextContent('exit 101')
    expect(row()).not.toHaveTextContent('running in background')
  })

  it('an answered question stays in the conversation with each question and its answer', () => {
    const view = buildView(
      evs([
        {
          type: 'permission',
          request_id: 'r1',
          kind: 'question',
          title: 'Agent has a question',
          detail: { questions: [{ id: 'q1', text: 'Which database?', choices: [{ id: 'sqlite', label: 'SQLite' }], free_text: true }] },
          options: [{ id: 'answer', label: 'Submit answers', kind: 'allow_once' }],
        },
        { type: 'permission_resolved', request_id: 'r1', option_id: 'answer', by_device: 'me', answers: { q1: 'sqlite' } },
      ]),
    )
    render(<MessageList {...props({ blocks: view.blocks, deviceLabel: () => 'you' })} />)
    const card = screen.getByTestId('permission-card')
    expect(card).toHaveAttribute('data-state', 'resolved')
    expect(card).toHaveTextContent('Answered by you')
    expect(within(card).getByTestId('permission-answer')).toHaveTextContent('Which database?SQLite')
  })

  it('each finished turn has a “copy” that takes all of that turn’s reply text (not the tools, not the previous turn)', async () => {
    const user = userEvent.setup()
    const writeText = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue()
    const view = buildView(
      evs([
        { type: 'user_message', message_id: 'm1', content: 'first', by_device: 'me' },
        { type: 'turn_start', turn_id: 1, message_id: 'm1' },
        { type: 'text_delta', text: 'old answer' },
        { type: 'text_done' },
        { type: 'turn_done', turn_id: 1, reason: 'completed' },
        { type: 'user_message', message_id: 'm2', content: 'second', by_device: 'me' },
        { type: 'turn_start', turn_id: 2, message_id: 'm2' },
        { type: 'text_delta', text: 'Part **one**' },
        { type: 'text_done' },
        { type: 'tool_call', id: 't1', name: 'Bash', category: 'shell', title: 'ls', input: { command: 'ls' }, detail: { command: 'ls' } },
        { type: 'tool_result', id: 't1', output: 'a\n', is_error: false },
        { type: 'text_delta', text: 'Part two' },
        { type: 'text_done' },
        { type: 'turn_done', turn_id: 2, reason: 'completed' },
      ]),
    )
    render(<MessageList {...props({ blocks: view.blocks })} />)
    const copies = screen.getAllByTestId('turn-copy')
    expect(copies).toHaveLength(2)
    expect(copies[1]).toHaveAccessibleName('Copy this reply')
    await user.click(copies[1]!)
    expect(writeText).toHaveBeenCalledWith('Part **one**\n\nPart two')
    await user.click(copies[0]!)
    expect(writeText).toHaveBeenLastCalledWith('old answer')
    writeText.mockRestore()
  })

  it('renders the event stream: user bubble, thinking, markdown reply, tool block with output, usage footer', async () => {
    const view = buildView(
      evs([
        { type: 'user_message', message_id: 'm1', content: 'run echo', by_device: 'me' },
        { type: 'turn_start', turn_id: 1, message_id: 'm1' },
        { type: 'thinking_delta', text: 'hmm' },
        { type: 'thinking_done', duration_ms: 3000 },
        { type: 'text_delta', text: 'Done: **ok**' },
        { type: 'text_done' },
        { type: 'tool_call', id: 't1', name: 'Bash', category: 'shell', title: 'echo hi', input: { command: 'echo hi' }, detail: { command: 'echo hi' } },
        { type: 'tool_output_delta', id: 't1', text: 'hi\n' },
        { type: 'tool_result', id: 't1', output: 'hi\n', is_error: false, detail: { exit_code: 0 } },
        { type: 'turn_done', turn_id: 1, reason: 'completed', usage: { input_tokens: 1200, output_tokens: 340, cost_usd: 0.02, duration_ms: 4200, model: 'mock-large' } },
      ]),
    )
    render(<MessageList {...props({ blocks: view.blocks })} />)
    expect(screen.getByTestId('message-user')).toHaveTextContent('run echo')
    expect(screen.getByTestId('thinking-block')).toHaveTextContent('Thought for 3 s')
    const reply = screen.getByTestId('message-assistant')
    expect(within(reply).getByText('ok').tagName).toBe('STRONG')
    const tool = screen.getByTestId('tool-block')
    expect(tool).toHaveAttribute('data-category', 'shell')
    expect(tool).toHaveTextContent('$ echo hi')
    // 命令块默认收起，点标题行才展开
    expect(screen.queryByTestId('tool-output')).toBeNull()
    await userEvent.click(within(tool).getByRole('button', { expanded: false }))
    expect(screen.getByTestId('tool-output')).toHaveTextContent('hi')
    // 展开后有完整的命令和复制按钮
    expect(screen.getByTestId('tool-command')).toHaveTextContent('$ echo hi')
    expect(within(screen.getByTestId('tool-command')).getByRole('button', { name: 'Copy command' })).toBeInTheDocument()
    const usage = screen.getByTestId('turn-usage')
    expect(usage).toHaveTextContent('mock-large')
    expect(usage).toHaveTextContent('1.2k in')
    expect(usage).toHaveTextContent('340 out')
    expect(usage).toHaveTextContent('$0.02')
  })

  it('shows a small badge when the node truncated a tool output (and when live output was cut)', () => {
    const tool = (detail: Record<string, unknown>) =>
      buildView(
        evs([
          { type: 'tool_call', id: 't1', name: 'Bash', category: 'shell', title: 'cat big.log', input: { command: 'cat big.log' }, detail: { command: 'cat big.log' } },
          { type: 'tool_result', id: 't1', output: 'head\n…[truncated 236000 bytes]\ntail', is_error: false, detail: { exit_code: 0, ...detail } },
        ]),
      )
    const { rerender } = render(<MessageList {...props({ blocks: tool({ output_truncated_bytes: 236000 }).blocks })} />)
    expect(screen.getByTestId('tool-truncated')).toHaveTextContent(/236[,.\s]?000 bytes|236000/)
    expect(screen.queryByTestId('tool-live-truncated')).toBeNull()
    rerender(<MessageList {...props({ blocks: tool({ live_output_truncated: true }).blocks })} />)
    expect(screen.queryByTestId('tool-truncated')).toBeNull()
    expect(screen.getByTestId('tool-live-truncated')).toHaveTextContent('Live output truncated')
    rerender(<MessageList {...props({ blocks: tool({ output_truncated_bytes: 5, live_output_truncated: true }).blocks })} />)
    expect(screen.getByTestId('tool-truncated')).toBeInTheDocument()
    expect(screen.getByTestId('tool-live-truncated')).toBeInTheDocument()
    rerender(<MessageList {...props({ blocks: tool({}).blocks })} />)
    expect(screen.queryByTestId('tool-truncated')).toBeNull()
    expect(screen.queryByTestId('tool-live-truncated')).toBeNull()
  })

  it('omits usage fields the agent did not report', () => {
    const view = buildView(evs([{ type: 'turn_done', turn_id: 1, reason: 'completed', usage: { duration_ms: 0 } }]))
    render(<MessageList {...props({ blocks: view.blocks })} />)
    expect(screen.queryByTestId('turn-usage')).toBeNull()
  })

  it('renders pending user messages with an unsent badge and retry / edit / delete', async () => {
    const user = userEvent.setup()
    const onRetry = vi.fn()
    const onEdit = vi.fn()
    const onDelete = vi.fn()
    const pending: PendingMessage[] = [{ messageId: 'u1', content: 'written offline', createdAt: 1, state: 'unsent' }]
    render(<MessageList {...props({ pending, onRetryPending: onRetry, onEditPending: onEdit, onDeletePending: onDelete })} />)
    const bubble = screen.getByTestId('message-user')
    expect(bubble).toHaveAttribute('data-state', 'unsent')
    expect(screen.getByTestId('message-unsent-badge')).toHaveTextContent('Not sent')
    await user.click(screen.getByTestId('message-retry'))
    await user.click(screen.getByRole('button', { name: 'Edit' }))
    await user.click(screen.getByRole('button', { name: 'Delete' }))
    expect(onRetry).toHaveBeenCalledWith('u1')
    expect(onEdit).toHaveBeenCalledWith('u1')
    expect(onDelete).toHaveBeenCalledWith('u1')
  })

  it('labels a message the daemon queued for the next turn', () => {
    render(<MessageList {...props({ pending: [{ messageId: 'q1', content: 'next', createdAt: 1, state: 'queued' }] })} />)
    expect(screen.getByTestId('message-queued')).toHaveTextContent('after this turn ends')
  })

  it('renders a settings_changed system line and the plan card', () => {
    const view = buildView(
      [ev({ type: 'plan', entries: [{ content: 'step', status: 'in_progress' }] }), ev({ type: 'settings_changed', settings: { model: 'mock-small' } })],
      { model: 'mock-large' },
    )
    render(<MessageList {...props({ blocks: view.blocks, modelLabel: (id) => (id === 'mock-small' ? 'Mock Small' : id) })} />)
    expect(screen.getByTestId('plan-card')).toHaveTextContent('step')
    expect(screen.getByTestId('system-line')).toHaveTextContent('Model switched to Mock Small')
  })

  it('shows a permission card in the stream and wires the response callback', async () => {
    const user = userEvent.setup()
    const onRespond = vi.fn()
    const view = buildView(
      evs([{ type: 'permission', request_id: 'r1', kind: 'tool', title: 'Run command', detail: {}, options: [{ id: 'allow', label: 'Allow', kind: 'allow_once' }] }]),
    )
    render(<MessageList {...props({ blocks: view.blocks, onRespond })} />)
    await user.click(screen.getByTestId('permission-option-allow'))
    expect(onRespond).toHaveBeenCalledWith({ request_id: 'r1', option_id: 'allow', message: undefined })
  })

  it('offers loading earlier messages', async () => {
    const user = userEvent.setup()
    const onLoadOlder = vi.fn()
    render(<MessageList {...props({ hasMoreOlder: true, onLoadOlder })} />)
    await user.click(screen.getByTestId('load-older'))
    expect(onLoadOlder).toHaveBeenCalled()
  })

  it('keeps small sessions fully rendered (virtualization starts only past the threshold)', () => {
    const view = buildView(evs(Array.from({ length: VIRTUALIZE_THRESHOLD - 1 }, (_, i) => ({ type: 'user_message' as const, message_id: `m${i}`, content: `msg ${i}`, by_device: 'me' }))))
    render(<MessageList {...props({ blocks: view.blocks })} />)
    expect(screen.getAllByTestId('message-user')).toHaveLength(VIRTUALIZE_THRESHOLD - 1)
  })

  describe('retry button for recoverable errors', () => {
    it('is shown on the latest recoverable error and calls onRetryTurn', async () => {
      const user = userEvent.setup()
      const onRetryTurn = vi.fn()
      const view = buildView(evs([{ type: 'user_message', message_id: 'm1', content: 'go', by_device: 'me' }, { type: 'error', code: 'agent_crashed', message: 'crashed', recoverable: true }]))
      render(<MessageList {...props({ blocks: view.blocks, onRetryTurn })} />)
      await user.click(screen.getByRole('button', { name: 'Retry' }))
      expect(onRetryTurn).toHaveBeenCalledTimes(1)
    })

    it('is not shown for unrecoverable errors, for old errors, or without a handler', () => {
      const old = buildView(
        evs([
          { type: 'error', code: 'a', message: 'old', recoverable: true },
          { type: 'user_message', message_id: 'm2', content: 'again', by_device: 'me' },
        ]),
      )
      const { rerender } = render(<MessageList {...props({ blocks: old.blocks, onRetryTurn: () => {} })} />)
      expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull() // 之后有新的用户消息
      const fatal = buildView(evs([{ type: 'error', code: 'b', message: 'fatal', recoverable: false }]))
      rerender(<MessageList {...props({ blocks: fatal.blocks, onRetryTurn: () => {} })} />)
      expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull()
      const ok = buildView(evs([{ type: 'error', code: 'c', message: 'again', recoverable: true }]))
      rerender(<MessageList {...props({ blocks: ok.blocks })} />)
      expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull()
    })

    it('retryableErrorKey picks only the last recoverable error that nothing follows', () => {
      const v = buildView(evs([{ type: 'error', code: 'a', message: 'one', recoverable: true }, { type: 'error', code: 'b', message: 'two', recoverable: true }]))
      expect(retryableErrorKey(v.blocks)).toBe(v.blocks.at(-1)!.key)
      expect(retryableErrorKey([])).toBeNull()
    })
  })

  it('passes the dangerous-mode flag to mode_change permission cards', () => {
    const view = buildView(
      evs([{ type: 'permission', request_id: 'r9', kind: 'mode_change', title: 'Switch', detail: { mode: 'yolo' }, options: [{ id: 'allow', label: 'Allow', kind: 'allow_once' }] }]),
    )
    render(<MessageList {...props({ blocks: view.blocks, modeLabel: () => 'YOLO', isDangerousMode: (id) => id === 'yolo' })} />)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-danger', 'true')
  })
})

describe('shell command block', () => {
  it('keeps a long command whole when expanded (the title line truncates it) and copies the command without the `$ `', async () => {
    const command = 'sed -n 520,659p lib/analytics.ts; sed -n 225,270p lib/analytics.ts; grep -n "AppState\\|background" lib/analytics.ts lib/appLifecycle.ts | head -30'
    const writeText = vi.fn(() => Promise.resolve())
    const user = userEvent.setup()
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true })
    const view = buildView(
      evs([
        { type: 'turn_start', turn_id: 1, message_id: 'm1' },
        { type: 'tool_call', id: 't1', name: 'Bash', category: 'shell', title: 'Read track()', input: { command }, detail: { command } },
      ]),
    )
    render(<MessageList {...props({ blocks: view.blocks })} />)
    const tool = screen.getByTestId('tool-block')
    expect(within(tool).getByTitle(command)).toBeInTheDocument()
    await user.click(within(tool).getByRole('button', { expanded: false }))
    expect(screen.getByTestId('tool-command')).toHaveTextContent(command)
    // 还在跑、没有输出：只有命令，没有空的输出区
    expect(screen.queryByTestId('tool-output')).toBeNull()
    await user.click(screen.getByRole('button', { name: 'Copy command' }))
    expect(writeText).toHaveBeenCalledWith(command)
  })

  it('uses the agent’s own description of the command as the collapsed title, and still shows the command when expanded', async () => {
    const user = userEvent.setup()
    const view = buildView(
      evs([
        { type: 'turn_start', turn_id: 1, message_id: 'm1' },
        { type: 'tool_call', id: 't1', name: 'Bash', category: 'shell', title: 'ls -la', input: { command: 'ls -la', description: 'List the files' }, detail: { command: 'ls -la' } },
        { type: 'tool_result', id: 't1', output: 'a\nb\n', is_error: false, detail: { exit_code: 0 } },
      ]),
    )
    render(<MessageList {...props({ blocks: view.blocks })} />)
    const tool = screen.getByTestId('tool-block')
    expect(tool).toHaveTextContent('List the files')
    expect(tool).not.toHaveTextContent('$ ls -la')
    await user.click(within(tool).getByRole('button', { expanded: false }))
    expect(screen.getByTestId('tool-command')).toHaveTextContent('$ ls -la')
    expect(screen.getByTestId('tool-output')).toHaveTextContent('a b')
  })

  it('adds a centred system line where the agent started a turn by itself (a background task finished); a turn for a user message gets none', () => {
    const view = buildView(
      evs([
        { type: 'user_message', message_id: 'm1', content: 'start the build', by_device: 'me' },
        { type: 'turn_start', turn_id: 1, message_id: 'm1' },
        { type: 'text_delta', text: 'Started in the background.' },
        { type: 'text_done' },
        { type: 'turn_done', turn_id: 1, reason: 'completed' },
        { type: 'turn_start', turn_id: 2, message_id: '' },
        { type: 'text_delta', text: 'The build finished.' },
        { type: 'text_done' },
        { type: 'turn_done', turn_id: 2, reason: 'completed' },
      ]),
    )
    render(<MessageList {...props({ blocks: view.blocks })} />)
    // 只有 agent 自己开的那一轮有提示，且它在这一轮的回复前面；普通 turn 的分隔线不变（没有文字）
    const line = screen.getByTestId('agent-turn-start')
    expect(line).toHaveTextContent('Background task finished — the agent continued')
    expect(screen.getAllByTestId('agent-turn-start')).toHaveLength(1)
    const reply = screen.getByText('The build finished.')
    expect(line.compareDocumentPosition(reply) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
    expect(screen.getAllByTestId('message-user')).toHaveLength(1)
    expect(screen.getByText('Started in the background.')).toBeInTheDocument()
  })

  describe('task, notice and activity', () => {
    const workflowCall = { type: 'tool_call', id: 'wf', name: 'Workflow', category: 'subagent', title: 'Workflow: review', input: { workflow: 'review' } } as const
    const task = (over: Partial<Extract<Event, { type: 'task' }>> = {}): Event => ({
      type: 'task',
      id: 'task-1',
      kind: 'workflow',
      tool_id: 'wf',
      title: 'review',
      status: 'running',
      phases: [{ index: 0, title: 'Scan' }],
      agents: [{ index: 0, label: 'scanner', state: 'running', phase_index: 0 }],
      ...over,
    })

    it('draws a task’s progress right under the tool line that started it, and updates it as snapshots arrive', () => {
      const first = buildView(evs([{ type: 'turn_start', turn_id: 1, message_id: 'm1' }, workflowCall, { type: 'tool_result', id: 'wf', output: 'started', is_error: false, detail: { background: true } }, task()]))
      const { rerender } = render(<MessageList {...props({ blocks: first.blocks })} />)
      const tool = screen.getByTestId('tool-block')
      const progress = screen.getByTestId('task')
      expect(tool.compareDocumentPosition(progress) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
      expect(screen.getAllByTestId('task')).toHaveLength(1)
      expect(screen.getByTestId('task-status')).toHaveTextContent('Running')
      expect(screen.getByTestId('task-agent')).toHaveAttribute('data-state', 'running')

      const done = applyEvents(first, [ev(task({ status: 'completed', agents: [{ index: 0, label: 'scanner', state: 'done', phase_index: 0 }] }))])
      rerender(<MessageList {...props({ blocks: done.blocks })} />)
      expect(screen.getAllByTestId('task')).toHaveLength(1)
      expect(screen.getByTestId('task-status')).toHaveTextContent('Completed')
    })

    // 真实录制里 Workflow 的调用结果是「Workflow launched in background. Task ID: …」、不带 background 标记
    const realWorkflowCall = { type: 'tool_call', id: 'toolu_wf', name: 'Workflow', category: 'other', title: 'Run workflow: alpha-beta', input: { script: 'x' } } as const
    const realLaunch = { type: 'tool_result', id: 'toolu_wf', output: 'Workflow launched in background. Task ID: whvf25gph', is_error: false } as const
    const realTask = (status: 'running' | 'completed' | 'failed'): Event => ({ type: 'task', id: 'whvf25gph', kind: 'workflow', tool_id: 'toolu_wf', title: 'alpha-beta', status, phases: [], agents: [] })

    it('a Workflow call whose launch result is not marked background says so while its task runs, and is done when the task completes', () => {
      const running = buildView(evs([realWorkflowCall, realLaunch, realTask('running')]))
      const { rerender } = render(<MessageList {...props({ blocks: running.blocks })} />)
      const row = () => screen.getByTestId('tool-block')
      expect(row()).toHaveAttribute('data-state', 'background')
      expect(row()).toHaveTextContent('running in background')
      expect(within(row()).getByLabelText('Running')).toBeInTheDocument()

      rerender(<MessageList {...props({ blocks: applyEvents(running, [ev(realTask('completed'))]).blocks })} />)
      expect(row()).toHaveAttribute('data-state', 'done')
      expect(row()).not.toHaveTextContent('running in background')
      expect(within(row()).queryByLabelText('Running')).toBeNull()

      const failed = buildView(evs([realWorkflowCall, realLaunch, realTask('running'), realTask('failed')]))
      rerender(<MessageList {...props({ blocks: failed.blocks })} />)
      expect(row()).toHaveAttribute('data-state', 'error')
      expect(row()).toHaveTextContent('failed')
    })

    it('a running task nobody can advance (tasksLive false) leaves the tool line stopped, not spinning', () => {
      const view = buildView(evs([realWorkflowCall, realLaunch, realTask('running')]))
      render(<MessageList {...props({ blocks: view.blocks, tasksLive: false })} />)
      const row = screen.getByTestId('tool-block')
      expect(row).toHaveAttribute('data-state', 'stopped')
      expect(row).not.toHaveTextContent('running in background')
      expect(within(row).queryByLabelText('Running')).toBeNull()
    })

    it('a sub-agent started by spawnAgent (immediate result, status only in the task) stays running in the background until its task ends', () => {
      const spawn = { type: 'tool_call', id: 'call_1', name: 'spawnAgent', category: 'subagent', title: 'Start sub-agent pong', input: {} } as const
      const result = { type: 'tool_result', id: 'call_1', output: 'Started sub-agent pong (fixture-model-sub)', is_error: false } as const
      const child = (status: 'running' | 'completed'): Event => ({ type: 'task', id: 'child', kind: 'subagent', tool_id: 'call_1', title: 'pong', status })
      const first = buildView(evs([spawn, result, child('running')]))
      const { rerender } = render(<MessageList {...props({ blocks: first.blocks })} />)
      const row = () => screen.getByTestId('tool-block')
      expect(row()).toHaveAttribute('data-category', 'subagent')
      expect(row()).toHaveAttribute('data-state', 'background')
      expect(row()).toHaveTextContent('running in background')
      // 立刻返回的那条结果只是「已启动」，不当作子 agent 的总结
      expect(screen.queryByTestId('subagent-result')).toBeNull()

      rerender(<MessageList {...props({ blocks: applyEvents(first, [ev(child('completed'))]).blocks })} />)
      expect(row()).toHaveAttribute('data-state', 'done')
      expect(row()).not.toHaveTextContent('running in background')
      // 子 agent 又收到新输入、回到运行中
      rerender(<MessageList {...props({ blocks: applyEvents(applyEvents(first, [ev(child('completed'))]), [ev(child('running'))]).blocks })} />)
      expect(row()).toHaveAttribute('data-state', 'background')
    })

    it('a task whose tool call is not loaded gets its own block where it first appeared', () => {
      const view = buildView(evs([{ type: 'text_delta', text: 'before' }, task({ tool_id: 'not-loaded' }), { type: 'text_delta', text: 'after' }]))
      render(<MessageList {...props({ blocks: view.blocks })} />)
      const rows = [...screen.getByTestId('message-list').querySelectorAll(':scope > div > div')]
      expect(rows.map((r) => (r.querySelector('[data-testid]') as HTMLElement | null)?.dataset.testid)).toEqual(['message-assistant', 'task', 'message-assistant'])
    })

    it('shows a snapshot stuck on running as stopped unless tasksLive says something may still be running it', () => {
      const view = buildView(evs([workflowCall, task()]))
      const { rerender } = render(<MessageList {...props({ blocks: view.blocks, tasksLive: false })} />)
      expect(screen.getByTestId('task')).toHaveAttribute('data-status', 'stopped')
      rerender(<MessageList {...props({ blocks: view.blocks, tasksLive: true })} />)
      expect(screen.getByTestId('task')).toHaveAttribute('data-status', 'running')
    })

    it('notices are lines in the stream; command output is a block; nothing looks like an error', () => {
      const view = buildView(
        evs([
          { type: 'notice', code: 'model_fallback', level: 'warning', message: 'overloaded', model_from: 'mock-large', model_to: 'mock-small' },
          { type: 'notice', code: 'command_output', level: 'info', message: 'Context: 6%' },
          { type: 'notice', code: 'agent_notice', level: 'info', message: 'Heads up' },
        ]),
      )
      render(<MessageList {...props({ blocks: view.blocks, modelLabel: (id) => id.replace('mock-', 'Mock ') })} />)
      const lines = screen.getAllByTestId('notice')
      expect(lines.map((l) => l.textContent)).toEqual(['Model switched from Mock large to Mock small — overloaded', 'Heads up'])
      expect(screen.getByTestId('notice-command-output')).toHaveTextContent('Context: 6%')
      expect(screen.queryByTestId('error-block')).toBeNull()
    })

    it('shows the activity line last while one is given, and not otherwise', () => {
      const view = buildView(evs([{ type: 'user_message', message_id: 'm1', content: 'go', by_device: 'me' }, { type: 'turn_start', turn_id: 1, message_id: 'm1' }, { type: 'activity', state: 'compacting' }]))
      const { rerender } = render(<MessageList {...props({ blocks: view.blocks, activity: view.activity })} />)
      const rows = [...screen.getByTestId('message-list').querySelectorAll(':scope > div > div')]
      expect(rows.at(-1)).toContainElement(screen.getByTestId('activity'))
      expect(screen.getByTestId('activity')).toHaveTextContent('Compacting context…')
      rerender(<MessageList {...props({ blocks: view.blocks, activity: null })} />)
      expect(screen.queryByTestId('activity')).toBeNull()
    })
  })
})
