import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { ev, evs, resetSeq } from '@/test/factories'
import type { PendingMessage } from '@/stores/drafts'
import { buildView } from '../reducer'
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
})
