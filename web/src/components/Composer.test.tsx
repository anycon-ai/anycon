import { fireEvent, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { useDrafts } from '@/stores/drafts'
import { usePrefs } from '@/stores/prefs'
import { Composer } from './Composer'

beforeEach(() => {
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {} })
  usePrefs.setState({ enterBehavior: 'send' })
})

describe('Composer', () => {
  it('sends on Enter, keeps Shift+Enter as a newline, and clears the draft', async () => {
    const onSend = vi.fn()
    const user = userEvent.setup()
    render(<Composer draftKey="s1" onSend={onSend} />)
    const input = screen.getByTestId('composer-input')
    await user.type(input, 'hello{Shift>}{Enter}{/Shift}world')
    expect(input).toHaveValue('hello\nworld')
    await user.keyboard('{Enter}')
    expect(onSend).toHaveBeenCalledWith('hello\nworld', [])
    expect(input).toHaveValue('')
  })

  it('does not send empty or blocked messages', async () => {
    const onSend = vi.fn()
    const user = userEvent.setup()
    const { rerender } = render(<Composer draftKey="s1" onSend={onSend} />)
    expect(screen.getByTestId('composer-send')).toBeDisabled()
    await user.type(screen.getByTestId('composer-input'), 'x')
    expect(screen.getByTestId('composer-send')).toBeEnabled()
    rerender(<Composer draftKey="s1" onSend={onSend} blockedReason="Node is offline" />)
    expect(screen.getByTestId('composer-send')).toBeDisabled()
    expect(screen.getByTestId('composer-blocked')).toHaveTextContent('Node is offline')
    // 阻塞时输入框仍然可用
    expect(screen.getByTestId('composer-input')).toBeEnabled()
  })

  it('in newline mode Enter inserts a line break and Ctrl+Enter sends', async () => {
    usePrefs.setState({ enterBehavior: 'newline' })
    const onSend = vi.fn()
    const user = userEvent.setup()
    render(<Composer draftKey="s1" onSend={onSend} />)
    const input = screen.getByTestId('composer-input')
    await user.type(input, 'a{Enter}b')
    expect(onSend).not.toHaveBeenCalled()
    await user.keyboard('{Control>}{Enter}{/Control}')
    expect(onSend).toHaveBeenCalledWith('a\nb', [])
  })

  it('shows Stop while running and labels typed text as queued for the next turn', async () => {
    const onStop = vi.fn()
    const user = userEvent.setup()
    render(<Composer draftKey="s1" onSend={() => {}} onStop={onStop} running queueNotice />)
    await user.click(screen.getByTestId('composer-stop'))
    expect(onStop).toHaveBeenCalled()
    expect(screen.queryByTestId('composer-queue-notice')).toBeNull()
    await user.type(screen.getByTestId('composer-input'), 'follow-up')
    expect(screen.getByTestId('composer-queue-notice')).toHaveTextContent(/after this turn/i)
  })

  it('Esc while running arms the Stop button, a second Esc stops', async () => {
    const onStop = vi.fn()
    const user = userEvent.setup()
    render(<Composer draftKey="s1" onSend={() => {}} onStop={onStop} running />)
    await user.click(screen.getByTestId('composer-input'))
    await user.keyboard('{Escape}')
    expect(screen.getByTestId('composer-stop')).toHaveAttribute('data-armed', 'true')
    expect(screen.getByTestId('composer-stop')).toHaveFocus()
    expect(onStop).not.toHaveBeenCalled()
    await user.keyboard('{Escape}')
    expect(onStop).toHaveBeenCalledTimes(1)
  })

  it('a held-down Esc (auto-repeat) arms the Stop button but never confirms it', async () => {
    const onStop = vi.fn()
    const user = userEvent.setup()
    render(<Composer draftKey="s1" onSend={() => {}} onStop={onStop} running />)
    await user.click(screen.getByTestId('composer-input'))
    // 第一下装填；后面的 repeat 事件不算
    fireEvent.keyDown(screen.getByTestId('composer-input'), { key: 'Escape' })
    expect(screen.getByTestId('composer-stop')).toHaveAttribute('data-armed', 'true')
    for (let i = 0; i < 5; i++) fireEvent.keyDown(screen.getByTestId('composer-stop'), { key: 'Escape', repeat: true })
    fireEvent.keyDown(screen.getByTestId('composer-input'), { key: 'Escape', repeat: true })
    expect(onStop).not.toHaveBeenCalled()
    // 松开再按一次才算确认
    fireEvent.keyDown(screen.getByTestId('composer-stop'), { key: 'Escape' })
    expect(onStop).toHaveBeenCalledTimes(1)
  })

  it('a held-down Enter sends once, not once per repeat', async () => {
    const onSend = vi.fn()
    const user = userEvent.setup()
    render(<Composer draftKey="s1" onSend={onSend} />)
    const input = screen.getByTestId('composer-input')
    await user.type(input, 'hello')
    fireEvent.keyDown(input, { key: 'Enter', repeat: true })
    expect(onSend).not.toHaveBeenCalled()
    fireEvent.keyDown(input, { key: 'Enter' })
    expect(onSend).toHaveBeenCalledTimes(1)
  })

  it('does not offer Stop when idle', () => {
    render(<Composer draftKey="s1" onSend={() => {}} onStop={() => {}} />)
    expect(screen.queryByTestId('composer-stop')).toBeNull()
  })

  it('persists the draft per key and restores it', async () => {
    const user = userEvent.setup()
    const { unmount } = render(<Composer draftKey="s1" onSend={() => {}} />)
    await user.type(screen.getByTestId('composer-input'), 'draft text')
    unmount()
    render(<Composer draftKey="s1" onSend={() => {}} />)
    expect(screen.getByTestId('composer-input')).toHaveValue('draft text')
    render(<Composer draftKey="s2" onSend={() => {}} />)
    expect(screen.getAllByTestId('composer-input')[1]).toHaveValue('')
  })

  it('renders the options bar inline on wide screens', () => {
    render(<Composer draftKey="s1" onSend={() => {}} options={(layout) => <div data-testid="opts">{layout}</div>} />)
    expect(screen.getByTestId('opts')).toHaveTextContent('inline')
  })
})
