import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import type { ActivityInfo, NoticeBlock } from '../reducer'
import { ActivityLine, NoticeLine, retryCountdown } from './Notices'

beforeEach(async () => {
  await i18n.changeLanguage('en')
})
afterEach(() => vi.useRealTimers())

const notice = (over: Partial<NoticeBlock>): NoticeBlock => ({ key: 'n1', seq: 1, ts: 1, type: 'notice', code: 'agent_notice', level: 'info', message: '', ...over })
const label = (id: string): string => `<${id}>`

describe('NoticeLine', () => {
  it('is a quiet status line, not an error block: no alert role, no retry', () => {
    render(<NoticeLine block={notice({ code: 'context_compacted', message: '' })} modelLabel={label} />)
    const line = screen.getByTestId('notice')
    expect(line).toHaveAttribute('role', 'status')
    expect(line).toHaveAttribute('data-level', 'info')
    expect(line).toHaveTextContent('Context was compacted')
    expect(line).toHaveClass('text-fg-subtle')
    expect(screen.queryByRole('alert')).toBeNull()
    expect(screen.queryByTestId('error-block')).toBeNull()
    expect(screen.queryByRole('button')).toBeNull()
  })

  it('draws warnings in the warning color', () => {
    render(<NoticeLine block={notice({ code: 'config_warning', level: 'warning', message: 'bad key in settings.json' })} modelLabel={label} />)
    expect(screen.getByTestId('notice')).toHaveClass('text-warning')
    expect(screen.getByTestId('notice')).toHaveAttribute('data-level', 'warning')
  })

  it('model_fallback names the models through the agent’s labels and keeps the agent’s wording after it; opening shows it in full', async () => {
    const user = userEvent.setup()
    render(<NoticeLine block={notice({ code: 'model_fallback', level: 'warning', message: 'Model mock-large is overloaded; switched to mock-small.', modelFrom: 'mock-large', modelTo: 'mock-small' })} modelLabel={label} />)
    const line = screen.getByTestId('notice')
    expect(line).toHaveTextContent('Model switched from <mock-large> to <mock-small> — Model mock-large is overloaded; switched to mock-small.')
    expect(screen.queryByTestId('notice-detail')).toBeNull()
    await user.click(screen.getByRole('button', { expanded: false }))
    expect(screen.getByTestId('notice-detail')).toHaveTextContent('Model mock-large is overloaded; switched to mock-small.')
  })

  it('shows rate_limit’s reset time in local time', () => {
    const at = new Date(2030, 0, 15, 9, 30).getTime()
    render(<NoticeLine block={notice({ code: 'rate_limit', level: 'warning', message: '', resetsAtMs: at })} modelLabel={label} />)
    expect(screen.getByTestId('notice')).toHaveTextContent(`Usage limit reached — resets ${new Date(at).toLocaleString('en', { dateStyle: 'medium', timeStyle: 'short' })}`)
  })

  it('is written in Chinese when the UI language is Chinese', async () => {
    await i18n.changeLanguage('zh')
    render(<NoticeLine block={notice({ code: 'model_fallback', level: 'warning', message: '', modelFrom: 'a', modelTo: 'b' })} modelLabel={(id) => id} />)
    expect(screen.getByTestId('notice')).toHaveTextContent('模型已从 a 换成 b')
  })

  it('shows an unknown code’s message as it is, at its level', () => {
    render(<NoticeLine block={notice({ code: 'brand_new', level: 'warning', message: 'Something new happened' })} modelLabel={label} />)
    expect(screen.getByTestId('notice')).toHaveTextContent('Something new happened')
    expect(screen.getByTestId('notice')).toHaveClass('text-warning')
  })

  it('command_output is a monospace block that can be folded', async () => {
    const user = userEvent.setup()
    render(<NoticeLine block={notice({ code: 'command_output', message: 'Context usage\n  system prompt  3.2k\n  messages       41k\n' })} modelLabel={label} />)
    const block = screen.getByTestId('notice-command-output')
    expect(block).toHaveTextContent('Command output')
    const out = screen.getByTestId('notice-output')
    expect(out.tagName).toBe('PRE')
    expect(out).toHaveClass('font-mono')
    expect(out.textContent).toBe('Context usage\n  system prompt  3.2k\n  messages       41k')
    await user.click(screen.getByRole('button', { name: /Command output/ }))
    expect(screen.queryByTestId('notice-output')).toBeNull()
    await user.click(screen.getByRole('button', { name: /Command output/ }))
    expect(screen.getByTestId('notice-output')).toBeInTheDocument()
  })

  it('command_output with no text has nothing to fold', () => {
    render(<NoticeLine block={notice({ code: 'command_output', message: '  ' })} modelLabel={label} />)
    expect(screen.getByRole('button')).toBeDisabled()
    expect(screen.queryByTestId('notice-output')).toBeNull()
  })
})

describe('retryCountdown', () => {
  const at = (over: Partial<ActivityInfo> = {}): Pick<ActivityInfo, 'state' | 'delayMs' | 'ts'> => ({ state: 'retrying', delayMs: 5000, ts: 10_000, ...over })
  it('counts down from the event time and rounds up', () => {
    expect(retryCountdown(at(), 10_000)).toBe(5)
    expect(retryCountdown(at(), 10_001)).toBe(5)
    expect(retryCountdown(at(), 12_500)).toBe(3)
    expect(retryCountdown(at(), 14_999)).toBe(1)
  })
  it('is null once the time is up, without a delay, or for other states', () => {
    expect(retryCountdown(at(), 15_000)).toBeNull()
    expect(retryCountdown(at(), 99_000)).toBeNull()
    expect(retryCountdown(at({ delayMs: undefined }), 10_000)).toBeNull()
    expect(retryCountdown(at({ delayMs: 0 }), 10_000)).toBeNull()
    expect(retryCountdown(at({ state: 'compacting' }), 10_000)).toBeNull()
  })
  it('never shows more than the delay the agent reported, even when the clocks disagree', () => {
    expect(retryCountdown(at(), 0)).toBe(5)
  })
})

describe('ActivityLine', () => {
  const retrying: ActivityInfo = { state: 'retrying', attempt: 2, maxAttempts: 10, delayMs: 5000, reason: '529 overloaded_error', seq: 3, ts: Date.now() }

  it('shows the attempt, a live countdown and the reason', () => {
    vi.useFakeTimers()
    vi.setSystemTime(1_000_000)
    render(<ActivityLine activity={{ ...retrying, ts: 1_000_000 }} />)
    const line = screen.getByTestId('activity')
    expect(line).toHaveAttribute('data-state', 'retrying')
    expect(line).toHaveTextContent('Retrying (2/10) in 5s…')
    expect(screen.getByTestId('activity-reason')).toHaveTextContent('529 overloaded_error')
    act(() => void vi.advanceTimersByTime(2000))
    expect(line).toHaveTextContent('Retrying (2/10) in 3s…')
    act(() => void vi.advanceTimersByTime(3000))
    expect(line).toHaveTextContent('Retrying (2/10)…')
    expect(line).not.toHaveTextContent(' in ')
  })

  it('tells screen readers the sentence without the ticking number', () => {
    vi.useFakeTimers()
    vi.setSystemTime(1_000_000)
    render(<ActivityLine activity={{ ...retrying, ts: 1_000_000 }} />)
    const line = screen.getByTestId('activity')
    expect(line).toHaveAttribute('role', 'status')
    const hidden = line.querySelector('[aria-hidden="true"]:not(svg)')
    expect(hidden).toHaveTextContent('Retrying (2/10) in 5s…')
    expect(line.querySelector('.sr-only')).toHaveTextContent('Retrying (2/10)…')
  })

  it('falls back to a shorter sentence when the agent reports less', () => {
    vi.useFakeTimers()
    vi.setSystemTime(0)
    const { rerender } = render(<ActivityLine activity={{ state: 'retrying', delayMs: 4000, seq: 1, ts: 0 }} />)
    expect(screen.getByTestId('activity')).toHaveTextContent('Retrying in 4s…')
    rerender(<ActivityLine activity={{ state: 'retrying', attempt: 3, maxAttempts: 5, seq: 1, ts: 0 }} />)
    expect(screen.getByTestId('activity')).toHaveTextContent('Retrying (3/5)…')
    expect(screen.queryByTestId('activity-reason')).toBeNull()
    rerender(<ActivityLine activity={{ state: 'retrying', seq: 1, ts: 0 }} />)
    expect(screen.getByTestId('activity')).toHaveTextContent('Retrying…')
  })

  it('compacting says so', () => {
    render(<ActivityLine activity={{ state: 'compacting', seq: 1, ts: 1 }} />)
    expect(screen.getByTestId('activity')).toHaveTextContent('Compacting context…')
  })

  it('is written in Chinese when the UI language is Chinese', async () => {
    await i18n.changeLanguage('zh')
    vi.useFakeTimers()
    vi.setSystemTime(1_000_000)
    render(<ActivityLine activity={{ ...retrying, ts: 1_000_000 }} />)
    expect(screen.getByTestId('activity')).toHaveTextContent('正在重试（2/10），5 秒后…')
  })
})
