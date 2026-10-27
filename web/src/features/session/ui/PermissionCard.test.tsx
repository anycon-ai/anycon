import { act, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import type { PermissionBlock } from '../reducer'
import { PermissionCard } from './PermissionCard'

const base: PermissionBlock = {
  type: 'permission',
  key: 'perm-r1',
  seq: 1,
  ts: 1,
  requestId: 'r1',
  kind: 'tool',
  title: 'Run command',
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: 'echo hi' }, cwd: '/work', detail: { command: 'echo hi' } } },
  options: [
    { id: 'allow', label: 'Allow', kind: 'allow_once' },
    { id: 'allow_session', label: 'Allow for this session', kind: 'allow_always' },
    { id: 'deny', label: 'Deny', kind: 'reject_once', needs_message: true },
  ],
}
const label = (id: string): string => (id === 'me' ? 'you' : 'iPhone')

describe('PermissionCard', () => {
  it('renders one button per option from options[], colored by kind, with stable test ids', () => {
    render(<PermissionCard block={base} deviceLabel={label} onRespond={() => {}} />)
    expect(screen.getByTestId('permission-card')).toBeInTheDocument()
    expect(screen.getByTestId('permission-command')).toHaveTextContent('$ echo hi')
    for (const o of base.options) {
      const b = screen.getByTestId(`permission-option-${o.id}`)
      expect(b).toHaveTextContent(o.label)
      expect(b).toHaveAttribute('data-kind', o.kind)
    }
    expect(screen.getAllByRole('button').filter((b) => b.dataset.testid?.startsWith('permission-option-') || b.getAttribute('data-testid')?.startsWith('permission-option-'))).toHaveLength(3)
  })

  it('does not hard-code the buttons: an ACP-style two-option request renders just those two', () => {
    const acp: PermissionBlock = { ...base, options: [{ id: 'opt-yes', label: 'Yes please', kind: 'allow_once' }, { id: 'opt-no', label: 'Nope', kind: 'reject_always' }] }
    render(<PermissionCard block={acp} deviceLabel={label} onRespond={() => {}} />)
    expect(screen.getByTestId('permission-option-opt-yes')).toBeInTheDocument()
    expect(screen.getByTestId('permission-option-opt-no')).toBeInTheDocument()
    expect(screen.queryByTestId('permission-option-allow')).toBeNull()
  })

  it('answers with the chosen option id and attaches the note only to options that accept one', async () => {
    const onRespond = vi.fn()
    const user = userEvent.setup()
    render(<PermissionCard block={base} deviceLabel={label} onRespond={onRespond} />)
    await user.type(screen.getByTestId('permission-note'), 'too risky')
    await user.click(screen.getByTestId('permission-option-allow'))
    expect(onRespond).toHaveBeenLastCalledWith({ request_id: 'r1', option_id: 'allow', message: undefined })
    await user.click(screen.getByTestId('permission-option-deny'))
    expect(onRespond).toHaveBeenLastCalledWith({ request_id: 'r1', option_id: 'deny', message: 'too risky' })
  })

  describe('an answered question', () => {
    const question: PermissionBlock = {
      ...base,
      kind: 'question',
      title: 'Agent has questions',
      detail: {
        questions: [
          { id: 'q1', text: 'Which database?', choices: [{ id: 'sqlite', label: 'SQLite' }, { id: 'pg', label: 'Postgres' }], free_text: true },
          { id: 'q2', text: 'Which features?', choices: [{ id: 'auth', label: 'Auth' }, { id: 'billing', label: 'Billing' }], multi: true },
          { id: 'q3', text: 'Anything else?', free_text: true },
          { id: 'q4', text: 'Deploy now?', choices: [{ id: 'yes', label: 'Yes' }] },
        ],
      },
      options: [
        { id: 'answer', label: 'Submit answers', kind: 'allow_once' },
        { id: 'deny', label: 'Skip', kind: 'reject_once', needs_message: true },
      ],
    }

    it('keeps each question with the answer: choice labels, several choices joined, free text as typed, unanswered ones marked', () => {
      render(<PermissionCard block={{ ...question, resolved: { optionId: 'answer', byDevice: 'me', ts: 2, answers: { q1: 'pg', q2: ['auth', 'billing'], q3: 'Keep it small\nplease' } } }} deviceLabel={label} onRespond={() => {}} />)
      const card = screen.getByTestId('permission-card')
      expect(card).toHaveAttribute('data-state', 'resolved')
      expect(card).toHaveTextContent('Agent has questions — Answered by you')
      const rows = within(card).getAllByTestId('permission-answer')
      expect(rows.map((r) => [r.dataset.question, r.querySelector('dt')?.textContent, r.querySelector('dd')?.textContent])).toEqual([
        ['q1', 'Which database?', 'Postgres'],
        ['q2', 'Which features?', 'Auth, Billing'],
        ['q3', 'Anything else?', 'Keep it small\nplease'],
        ['q4', 'Deploy now?', 'No answer'],
      ])
      expect(screen.queryByRole('radio')).toBeNull()
      expect(screen.queryByTestId('permission-option-answer')).toBeNull()
    })

    it('says a skipped question was skipped, and shows the note sent with it', () => {
      render(<PermissionCard block={{ ...question, resolved: { optionId: 'deny', byDevice: 'iphone', ts: 2, message: 'Ask me later' } }} deviceLabel={label} onRespond={() => {}} />)
      const card = screen.getByTestId('permission-card')
      expect(card).toHaveTextContent('Skipped by iPhone')
      expect(within(card).getByTestId('permission-message')).toHaveTextContent('Ask me later')
      expect(within(card).queryByTestId('permission-answers')).toBeNull()
    })

    it('is a single line for replies recorded without answers (older history)', () => {
      render(<PermissionCard block={{ ...question, resolved: { optionId: 'answer', byDevice: 'me', ts: 2 } }} deviceLabel={label} onRespond={() => {}} />)
      const card = screen.getByTestId('permission-card')
      expect(card.tagName).toBe('P')
      expect(card).toHaveTextContent('Agent has questions — Answered by you')
    })

    it('in Chinese', async () => {
      await i18n.changeLanguage('zh')
      try {
        render(<PermissionCard block={{ ...question, resolved: { optionId: 'answer', byDevice: 'me', ts: 2, answers: { q1: 'sqlite' } } }} deviceLabel={() => '你'} onRespond={() => {}} />)
        expect(screen.getByTestId('permission-card')).toHaveTextContent('已由 你 回答')
        expect(screen.getAllByTestId('permission-answer')[1]).toHaveTextContent('未回答')
      } finally {
        await i18n.changeLanguage('en')
      }
    })
  })

  it('shows the reason given with a denial under the line', () => {
    render(<PermissionCard block={{ ...base, resolved: { optionId: 'deny', byDevice: 'me', ts: 2, message: 'too risky' } }} deviceLabel={label} onRespond={() => {}} />)
    const card = screen.getByTestId('permission-card')
    expect(card).toHaveTextContent('Denied by you (Deny)')
    expect(within(card).getByTestId('permission-message')).toHaveTextContent('too risky')
  })

  it('disables the buttons while an answer is in flight', () => {
    render(<PermissionCard block={base} deviceLabel={label} onRespond={() => {}} busy />)
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
  })

  it('collapses to “Allowed by <device>” once another device resolved it', () => {
    render(<PermissionCard block={{ ...base, resolved: { optionId: 'allow', byDevice: 'iphone', ts: 2 } }} deviceLabel={label} onRespond={() => {}} />)
    const card = screen.getByTestId('permission-card')
    expect(card).toHaveAttribute('data-state', 'resolved')
    expect(card).toHaveTextContent('Allowed by iPhone')
    expect(screen.queryByTestId('permission-option-allow')).toBeNull()
  })

  it('shows a denial as denied', () => {
    render(<PermissionCard block={{ ...base, resolved: { optionId: 'deny', byDevice: 'me', ts: 2 } }} deviceLabel={label} onRespond={() => {}} />)
    expect(screen.getByTestId('permission-card')).toHaveTextContent('Denied by you')
  })

  describe('answered by the daemon itself (by_device: "daemon") — folder changed, timeout, cancel', () => {
    it.each([
      ['deny', 'Denied automatically by anycon'],
      ['allow', 'Allowed automatically by anycon'],
    ])('shows a %s from the daemon as “%s”, never the raw word “daemon” or an unknown device', (optionId, text) => {
      // deviceLabel 在这里故意是个会把 "daemon" 当设备名的实现：卡片自己要认出 daemon
      render(<PermissionCard block={{ ...base, resolved: { optionId, byDevice: 'daemon', ts: 2 } }} deviceLabel={(id) => id} onRespond={() => {}} />)
      const card = screen.getByTestId('permission-card')
      expect(card).toHaveAttribute('data-state', 'resolved')
      expect(card).toHaveAttribute('data-by', 'daemon')
      expect(card).toHaveTextContent(text)
      expect(card.textContent).not.toMatch(/daemon/i)
      expect(card.textContent).not.toMatch(/by iPhone|by you|unknown/i)
    })

    it('says “Handled automatically” when the daemon settled a question or gave no option', () => {
      const { rerender } = render(<PermissionCard block={{ ...base, resolved: { optionId: null, byDevice: 'daemon', ts: 2 } }} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.getByTestId('permission-card')).toHaveTextContent('Handled automatically by anycon')
      rerender(<PermissionCard block={{ ...base, kind: 'question', options: [], detail: { questions: [] }, resolved: { optionId: null, byDevice: 'daemon', ts: 2 } }} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.getByTestId('permission-card')).toHaveTextContent('Handled automatically by anycon')
    })

    it('has Chinese copy, and leaves answers from real devices as they were', async () => {
      await i18n.changeLanguage('zh')
      try {
        const { rerender } = render(<PermissionCard block={{ ...base, resolved: { optionId: 'deny', byDevice: 'daemon', ts: 2 } }} deviceLabel={label} onRespond={() => {}} />)
        expect(screen.getByTestId('permission-card')).toHaveTextContent('已被 anycon 自动拒绝')
        rerender(<PermissionCard block={{ ...base, resolved: { optionId: 'deny', byDevice: 'iphone', ts: 2 } }} deviceLabel={label} onRespond={() => {}} />)
        expect(screen.getByTestId('permission-card')).not.toHaveAttribute('data-by')
        expect(screen.getByTestId('permission-card')).not.toHaveTextContent('自动')
      } finally {
        await i18n.changeLanguage('en')
      }
    })
  })

  describe('settled (permission.respond answered not_found)', () => {
    it('folder_changed (data.reason = project_dir_changed): says the request was denied and the turn stopped, and offers no buttons, so the same request cannot be sent again', () => {
      render(<PermissionCard block={base} deviceLabel={label} onRespond={() => {}} settled="folder_changed" />)
      const card = screen.getByTestId('permission-card')
      expect(card).toHaveAttribute('data-state', 'settled')
      expect(card).toHaveAttribute('data-settled', 'folder_changed')
      expect(card).toHaveAttribute('role', 'status')
      expect(screen.getByTestId('permission-settled')).toHaveTextContent('This request was denied because the project folder changed, and the turn was stopped.')
      expect(screen.queryByTestId('permission-option-allow')).toBeNull()
      expect(screen.queryByRole('button')).toBeNull()
    })

    it('gone (any other not_found): only says the request no longer exists — nothing about the folder or the turn — and offers no buttons', () => {
      render(<PermissionCard block={base} deviceLabel={label} onRespond={() => {}} settled="gone" />)
      const card = screen.getByTestId('permission-card')
      expect(card).toHaveAttribute('data-state', 'settled')
      expect(card).toHaveAttribute('data-settled', 'gone')
      expect(screen.getByTestId('permission-settled')).toHaveTextContent('This request no longer exists, so it can’t be answered.')
      expect(card.textContent).not.toMatch(/folder|turn was stopped|denied/i)
      expect(screen.queryByRole('button')).toBeNull()
    })

    it('hands over to the resolved row once the daemon’s permission_resolved arrives', () => {
      render(<PermissionCard block={{ ...base, resolved: { optionId: 'deny', byDevice: 'daemon', ts: 2 } }} deviceLabel={label} onRespond={() => {}} settled="folder_changed" />)
      expect(screen.getByTestId('permission-card')).toHaveAttribute('data-state', 'resolved')
      expect(screen.queryByTestId('permission-settled')).toBeNull()
    })

    it('has Chinese copy for both', async () => {
      await i18n.changeLanguage('zh')
      try {
        const { rerender } = render(<PermissionCard block={base} deviceLabel={label} onRespond={() => {}} settled="folder_changed" />)
        expect(screen.getByTestId('permission-settled')).toHaveTextContent('这个请求已被拒绝：项目目录变了，本轮已中止。')
        rerender(<PermissionCard block={base} deviceLabel={label} onRespond={() => {}} settled="gone" />)
        expect(screen.getByTestId('permission-settled')).toHaveTextContent('这个请求已经不存在，无法再答复。')
      } finally {
        await i18n.changeLanguage('en')
      }
    })
  })

  it('renders a plan request as Markdown with the adapter-provided buttons', () => {
    const plan: PermissionBlock = {
      ...base,
      kind: 'plan',
      title: 'Plan ready',
      detail: { text: '## Steps\n\n- one\n- two' },
      options: [
        { id: 'accept', label: 'Start implementing', kind: 'allow_once' },
        { id: 'revise', label: 'Keep planning', kind: 'reject_once', needs_message: true },
      ],
    }
    render(<PermissionCard block={plan} deviceLabel={label} onRespond={() => {}} />)
    const text = within(screen.getByTestId('permission-plan'))
    expect(text.getByRole('heading', { name: 'Steps' })).toBeInTheDocument()
    expect(text.getAllByRole('listitem')).toHaveLength(2)
    expect(screen.getByTestId('permission-option-accept')).toHaveTextContent('Start implementing')
    expect(screen.getByTestId('permission-option-revise')).toBeInTheDocument()
  })

  it('collects answers for a question request and submits them all at once', async () => {
    const onRespond = vi.fn()
    const user = userEvent.setup()
    const q: PermissionBlock = {
      ...base,
      kind: 'question',
      title: 'Agent has questions',
      detail: {
        questions: [
          { id: 'q1', text: 'Which database?', choices: [{ id: 'pg', label: 'Postgres' }, { id: 'sqlite', label: 'SQLite' }] },
          { id: 'q2', text: 'Pick features', multi: true, choices: [{ id: 'a', label: 'Auth' }, { id: 'b', label: 'Billing' }] },
          { id: 'q3', text: 'Anything else?', free_text: true },
        ],
      },
      options: [],
    }
    render(<PermissionCard block={q} deviceLabel={label} onRespond={onRespond} />)
    const submit = screen.getByTestId('permission-submit-answers')
    expect(submit).toBeDisabled()
    await user.click(screen.getByLabelText('SQLite'))
    await user.click(screen.getByLabelText('Auth'))
    await user.click(screen.getByLabelText('Billing'))
    expect(submit).toBeDisabled()
    await user.type(screen.getByRole('textbox', { name: 'Anything else?' }), 'no')
    expect(submit).toBeEnabled()
    await user.click(submit)
    expect(onRespond).toHaveBeenCalledWith({ request_id: 'r1', answers: { q1: 'sqlite', q2: ['a', 'b'], q3: 'no' } })
  })

  it('uses the adapter’s allow option as the only submit button and sends the answers with it', async () => {
    const onRespond = vi.fn()
    const user = userEvent.setup()
    const q: PermissionBlock = {
      ...base,
      kind: 'question',
      title: 'Agent has a question',
      detail: { questions: [{ id: 'q1', text: 'Keep it?', choices: [{ id: 'keep', label: 'Keep' }, { id: 'drop', label: 'Drop' }], free_text: true }] },
      options: [
        { id: 'answer', label: 'Submit answers', kind: 'allow_once' },
        { id: 'deny', label: 'Skip', kind: 'reject_once', needs_message: true },
      ],
    }
    render(<PermissionCard block={q} deviceLabel={label} onRespond={onRespond} />)
    expect(screen.queryByTestId('permission-submit-answers')).not.toBeInTheDocument()
    expect(screen.getAllByRole('button', { name: 'Submit answers' })).toHaveLength(1)
    const submit = screen.getByTestId('permission-option-answer')
    expect(submit).toBeDisabled()
    expect(screen.getByTestId('permission-option-deny')).toBeEnabled()
    await user.click(screen.getByLabelText('Keep'))
    await user.click(submit)
    expect(onRespond).toHaveBeenCalledWith({ request_id: 'r1', option_id: 'answer', answers: { q1: 'keep' } })
  })

  describe('optional questions (MCP elicitation forms)', () => {
    const form: PermissionBlock = {
      ...base,
      kind: 'question',
      title: 'example-elicit: Please enter your details',
      detail: {
        questions: [
          { id: 'name', text: 'Name — Your first name', free_text: true },
          { id: 'nick', text: 'Nickname', free_text: true, optional: true },
          { id: 'color', text: 'Favorite color', choices: [{ id: 'red', label: 'Red' }, { id: 'blue', label: 'Blue' }], optional: true },
        ],
      },
      options: [
        { id: 'accept', label: 'Submit', kind: 'allow_once' },
        { id: 'decline', label: 'Decline', kind: 'reject_once' },
      ],
    }

    it('marks only the optional questions, and submit is enabled once every required one is answered', async () => {
      const onRespond = vi.fn()
      const user = userEvent.setup()
      render(<PermissionCard block={form} deviceLabel={label} onRespond={onRespond} />)
      const hints = screen.getAllByTestId('question-optional')
      expect(hints.map((h) => h.textContent)).toEqual(['optional', 'optional'])
      expect(screen.getByRole('group', { name: /^Name/ })).not.toHaveTextContent('optional')
      expect(screen.getByRole('group', { name: /^Nickname/ })).toHaveTextContent('optional')
      expect(screen.queryByText(/required/i)).toBeNull()

      const submit = screen.getByTestId('permission-option-accept')
      expect(submit).toBeDisabled()
      await user.type(screen.getByRole('textbox', { name: 'Name — Your first name' }), 'Ada')
      expect(submit).toBeEnabled()
      await user.click(submit)
      // 没答的可选项不在 answers 里
      expect(onRespond).toHaveBeenCalledWith({ request_id: 'r1', option_id: 'accept', message: undefined, answers: { name: 'Ada' } })
      // 拒绝不受影响
      expect(screen.getByTestId('permission-option-decline')).toBeEnabled()
    })

    it('answered optional questions are sent, a cleared one is dropped, and a chosen option can be cleared again', async () => {
      const onRespond = vi.fn()
      const user = userEvent.setup()
      render(<PermissionCard block={form} deviceLabel={label} onRespond={onRespond} />)
      await user.type(screen.getByRole('textbox', { name: 'Name — Your first name' }), 'Ada')
      await user.type(screen.getByRole('textbox', { name: 'Nickname' }), 'x')
      await user.clear(screen.getByRole('textbox', { name: 'Nickname' }))
      await user.click(screen.getByLabelText('Red'))
      expect(screen.getByTestId('question-clear')).toBeInTheDocument()
      await user.click(screen.getByTestId('question-clear'))
      expect(screen.queryByTestId('question-clear')).toBeNull()
      expect(screen.getByLabelText('Red')).not.toBeChecked()
      await user.click(screen.getByLabelText('Blue'))
      await user.click(screen.getByTestId('permission-option-accept'))
      expect(onRespond).toHaveBeenCalledWith({ request_id: 'r1', option_id: 'accept', message: undefined, answers: { name: 'Ada', color: 'blue' } })
    })

    it('a form with only optional questions can be submitted empty; required (default) questions behave as before', async () => {
      const onRespond = vi.fn()
      const user = userEvent.setup()
      const allOptional: PermissionBlock = { ...form, detail: { questions: form.detail.questions!.slice(1) } }
      render(<PermissionCard block={allOptional} deviceLabel={label} onRespond={onRespond} />)
      const submit = screen.getByTestId('permission-option-accept')
      expect(submit).toBeEnabled()
      await user.click(submit)
      expect(onRespond).toHaveBeenCalledWith({ request_id: 'r1', option_id: 'accept', message: undefined, answers: {} })
    })

    it('without an adapter submit option the built-in button follows the same rule', async () => {
      const onRespond = vi.fn()
      const user = userEvent.setup()
      render(<PermissionCard block={{ ...form, options: [] }} deviceLabel={label} onRespond={onRespond} />)
      const submit = screen.getByTestId('permission-submit-answers')
      expect(submit).toBeDisabled()
      await user.type(screen.getByRole('textbox', { name: 'Name — Your first name' }), 'Ada')
      await user.click(submit)
      expect(onRespond).toHaveBeenCalledWith({ request_id: 'r1', answers: { name: 'Ada' } })
    })

    it('is written in Chinese when the UI language is Chinese', async () => {
      await i18n.changeLanguage('zh')
      try {
        render(<PermissionCard block={form} deviceLabel={label} onRespond={() => {}} />)
        expect(screen.getAllByTestId('question-optional').map((h) => h.textContent)).toEqual(['可选', '可选'])
      } finally {
        await i18n.changeLanguage('en')
      }
    })
  })

  describe('mode_change', () => {
    const modeChange = (mode: string): PermissionBlock => ({
      ...base,
      kind: 'mode_change',
      title: 'Switch mode',
      detail: { mode },
      options: [
        { id: 'allow', label: 'Allow', kind: 'allow_once' },
        { id: 'deny', label: 'Deny', kind: 'reject_once' },
      ],
    })

    it('gives a request for a dangerous mode a warning style, a warning text and a danger-coloured allow button', () => {
      render(<PermissionCard block={modeChange('yolo')} deviceLabel={label} onRespond={() => {}} targetMode={{ label: 'YOLO', dangerous: true }} />)
      expect(screen.getByTestId('permission-card')).toHaveAttribute('data-danger', 'true')
      expect(screen.getByTestId('permission-danger')).toHaveTextContent(/YOLO/)
      expect(screen.getByRole('alert')).toBeInTheDocument()
      expect(screen.getByTestId('permission-option-allow').className).toMatch(/danger/)
    })

    it('shows the mode name instead of the raw id, and no warning for a normal mode', () => {
      render(<PermissionCard block={modeChange('plan')} deviceLabel={label} onRespond={() => {}} targetMode={{ label: 'Plan Mode', dangerous: false }} />)
      expect(screen.getByTestId('permission-card')).not.toHaveAttribute('data-danger')
      expect(screen.queryByTestId('permission-danger')).toBeNull()
      expect(screen.getByTestId('permission-card')).toHaveTextContent('Plan Mode')
    })

    it('answering still uses the adapter’s option ids (no silent upgrade to an “always” option)', async () => {
      const onRespond = vi.fn()
      const user = userEvent.setup()
      render(<PermissionCard block={modeChange('yolo')} deviceLabel={label} onRespond={onRespond} targetMode={{ label: 'YOLO', dangerous: true }} />)
      await user.click(screen.getByTestId('permission-option-allow'))
      expect(onRespond).toHaveBeenCalledWith({ request_id: 'r1', option_id: 'allow', message: undefined })
    })
  })

  describe('expires_at_ms', () => {
    afterEach(() => vi.useRealTimers())

    it('counts down to the deadline and flags it once passed', () => {
      vi.useFakeTimers({ now: 1_000_000 })
      render(<PermissionCard block={{ ...base, expiresAtMs: 1_000_000 + 125_000 }} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.getByTestId('permission-expiry')).toHaveTextContent('2:05')
      act(() => {
        vi.advanceTimersByTime(60_000)
      })
      expect(screen.getByTestId('permission-expiry')).toHaveTextContent('1:05')
      act(() => {
        vi.advanceTimersByTime(70_000)
      })
      expect(screen.getByTestId('permission-expiry')).toHaveAttribute('data-expired', 'true')
    })

    it('shows nothing when the request has no deadline, and not on resolved cards', () => {
      const { rerender } = render(<PermissionCard block={base} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.queryByTestId('permission-expiry')).toBeNull()
      rerender(<PermissionCard block={{ ...base, expiresAtMs: Date.now() + 5000, resolved: { optionId: 'allow', byDevice: 'me', ts: 2 } }} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.queryByTestId('permission-expiry')).toBeNull()
    })
  })

  describe('detail.url (URL-mode authorization)', () => {
    const link: PermissionBlock = {
      ...base,
      requestId: 'r-link',
      kind: 'other',
      title: 'Open a link to continue',
      detail: { text: 'The mock-auth server needs you to sign in. Open the link, then confirm here.', url: 'https://example.com/mock-auth?state=abc123&redirect=https%3A%2F%2Fexample.com%2Fdone#section' },
      options: [
        { id: 'accept', label: "I've completed it", kind: 'allow_once' },
        { id: 'decline', label: 'Decline', kind: 'reject_once' },
      ],
    }

    it('shows the full URL and an Open button that opens it in a new tab without giving the page away', () => {
      render(<PermissionCard block={link} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.getByText(/needs you to sign in/)).toBeInTheDocument()
      // 完整的 URL：查询串和片段都在，不截断
      expect(screen.getByTestId('permission-link-url')).toHaveTextContent('https://example.com/mock-auth?state=abc123&redirect=https%3A%2F%2Fexample.com%2Fdone#section')
      const open = screen.getByTestId('permission-link-open')
      expect(open.tagName).toBe('A')
      expect(open).toHaveAttribute('href', 'https://example.com/mock-auth?state=abc123&redirect=https%3A%2F%2Fexample.com%2Fdone#section')
      expect(open).toHaveAttribute('target', '_blank')
      expect(open).toHaveAttribute('rel', 'noopener noreferrer')
      expect(open).toHaveTextContent('Open')
      expect(screen.queryByTestId('permission-link-blocked')).toBeNull()
    })

    it('opening the link does not answer the request; the user confirms with the option buttons', async () => {
      const onRespond = vi.fn()
      const user = userEvent.setup()
      render(<PermissionCard block={link} deviceLabel={label} onRespond={onRespond} />)
      // jsdom 不导航；点击不会触发答复
      screen.getByTestId('permission-link-open').addEventListener('click', (e) => e.preventDefault())
      await user.click(screen.getByTestId('permission-link-open'))
      expect(onRespond).not.toHaveBeenCalled()
      await user.click(screen.getByTestId('permission-option-accept'))
      expect(onRespond).toHaveBeenCalledWith({ request_id: 'r-link', option_id: 'accept', message: undefined })
    })

    it.each(['javascript:alert(1)', 'data:text/html,<script>alert(1)</script>', 'file:///etc/passwd', 'ftp://example.com/x', 'not a url'])('shows %s as plain text with no link at all', (url) => {
      render(<PermissionCard block={{ ...link, detail: { ...link.detail, url } }} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.getByTestId('permission-link-url')).toHaveTextContent(url)
      expect(screen.queryByTestId('permission-link-open')).toBeNull()
      expect(document.querySelector('a')).toBeNull()
      expect(screen.getByTestId('permission-link-blocked')).toHaveTextContent('Only http and https links can be opened')
      // 确认 / 拒绝的按钮照常
      expect(screen.getByTestId('permission-option-decline')).toBeInTheDocument()
    })

    it('has no link row without a url, and ignores a url on other kinds', () => {
      const { rerender } = render(<PermissionCard block={{ ...link, detail: { text: 'just confirm' } }} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.queryByTestId('permission-link')).toBeNull()
      rerender(<PermissionCard block={{ ...link, kind: 'plan', detail: { text: '# plan', url: 'https://example.com' } }} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.queryByTestId('permission-link')).toBeNull()
    })

    it('is written in Chinese when the UI language is Chinese', async () => {
      await i18n.changeLanguage('zh')
      try {
        render(<PermissionCard block={link} deviceLabel={label} onRespond={() => {}} />)
        expect(screen.getByTestId('permission-link-open')).toHaveTextContent('打开')
        expect(screen.getByTestId('permission-link-open')).toHaveAccessibleName('在新标签页中打开链接')
      } finally {
        await i18n.changeLanguage('en')
      }
    })

    it('an answered link request collapses to the usual one-line summary with no URL', () => {
      render(<PermissionCard block={{ ...link, resolved: { optionId: 'accept', byDevice: 'me', ts: 2 } }} deviceLabel={label} onRespond={() => {}} />)
      expect(screen.getByTestId('permission-card')).toHaveAttribute('data-state', 'resolved')
      expect(screen.queryByTestId('permission-link')).toBeNull()
    })
  })
})
