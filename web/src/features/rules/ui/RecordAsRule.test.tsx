// 「记成规则」（docs/spec/daemon.md § 23.6，spec/web.md § 4.14）：预填、agent 写的话和用户的选择分开显示、存下来的出处、
// 不答复请求、不改答复按钮、不在键盘快捷键的路径上；节点不支持 / 不在线时没有或禁用。
import { act, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { overlayOpen } from '@/features/inbox/useInboxKeys'
import type { PermissionBlock } from '@/features/session/reducer'
import { PermissionCard } from '@/features/session/ui/PermissionCard'
import { nodeEntry, project, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeRulesDaemon } from '@/test/fakeRulesDaemon'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { cancelRuleRefreshes, resetRulesMemory } from '../service'
import { useRules } from '../store'

const N = 'n'.repeat(32)
const info = (rules: boolean): NodeInfo => ({ capabilities: { rules } }) as unknown as NodeInfo

const tool: PermissionBlock = {
  type: 'permission',
  key: 'perm-r1',
  seq: 1,
  ts: 1,
  requestId: 'r1',
  kind: 'tool',
  title: 'Run the migration',
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: 'pnpm db:migrate' }, cwd: '/work', detail: { command: 'pnpm db:migrate' } } },
  options: [
    { id: 'allow', label: 'Allow once', kind: 'allow_once' },
    { id: 'deny', label: 'Deny', kind: 'reject_once' },
  ],
}
const question: PermissionBlock = {
  ...tool,
  requestId: 'q1',
  key: 'perm-q1',
  kind: 'question',
  title: 'Which package manager?',
  detail: { questions: [{ id: 'pm', text: 'Which package manager should I use?', choices: [{ id: 'pnpm', label: 'pnpm' }, { id: 'npm', label: 'npm' }] }] },
  options: [{ id: 'submit', label: 'Submit answers', kind: 'allow_once' }],
}

let rpc: FakeRpc
let daemon: FakeRulesDaemon
const onRespond = vi.fn()
const card = (block: PermissionBlock = tool) => <PermissionCard block={block} deviceLabel={() => 'you'} onRespond={onRespond} recordAsRule={{ nodeId: N, sessionId: 's1' }} />
const setNode = (over: Partial<Parameters<typeof nodeEntry>[0]> = {}): void => {
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'alpha', info: info(true), conn: 'ready', projects: [project({ name: 'web' })], ...over }) }, order: [N], loaded: true })
}
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 8; i++) await new Promise((r) => setTimeout(r, 5))
  })
}

beforeEach(() => {
  rpc = new FakeRpc()
  daemon = new FakeRulesDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  resetRulesMemory()
  onRespond.mockReset()
  useSessions.setState({ list: { s1: { ...summary({ id: 's1', project: 'web' }), node_id: N, cached_seq: 0 } as never }, pending: {}, open: {} })
  setNode()
})
afterEach(() => {
  cancelRuleRefreshes()
  setRpc(null)
})

describe('where the button is', () => {
  it('sits next to the answer buttons; it is not one of the options', () => {
    render(card())
    const options = screen.getAllByRole('button').filter((b) => (b.getAttribute('data-testid') ?? '').startsWith('permission-option-'))
    expect(options).toHaveLength(2)
    expect(screen.getByTestId('record-rule')).toBeInTheDocument()
    expect(options.includes(screen.getByTestId('record-rule'))).toBe(false)
  })

  it('is not there without a rules-capable node, or when the caller does not offer it', () => {
    setNode({ info: info(false) })
    const { unmount } = render(card())
    expect(screen.queryByTestId('record-rule')).toBeNull()
    unmount()
    setNode()
    render(<PermissionCard block={tool} deviceLabel={() => 'you'} onRespond={onRespond} />)
    expect(screen.queryByTestId('record-rule')).toBeNull()
  })

  it('is disabled while the node is offline', () => {
    setNode({ conn: 'peer_offline' })
    render(card())
    expect(screen.getByTestId('record-rule')).toBeDisabled()
  })

  it('is not there once the request has been answered', () => {
    render(<PermissionCard block={{ ...tool, resolved: { optionId: 'allow', byDevice: 'me', ts: 1 } }} deviceLabel={() => 'you'} onRespond={onRespond} recordAsRule={{ nodeId: N, sessionId: 's1' }} />)
    expect(screen.queryByTestId('record-rule')).toBeNull()
  })
})

describe('the editor for a permission request', () => {
  it('keeps what the agent wrote apart from your choice, and says a rule approves nothing', async () => {
    const user = userEvent.setup()
    render(card())
    await user.click(screen.getByTestId('record-rule'))
    const agent = screen.getByTestId('record-agent')
    expect(agent).toHaveTextContent('The agent wrote')
    expect(agent).toHaveTextContent('Run the migration')
    expect(agent).toHaveTextContent('pnpm db:migrate')
    const choice = screen.getByTestId('record-choice')
    expect(choice).toHaveTextContent('How you would answer. This only fills in the rule text.')
    expect(choice).not.toHaveTextContent('pnpm db:migrate')
    expect(agent).not.toContainElement(choice)
    expect(screen.getByTestId('rule-editor')).toHaveTextContent('A rule does not approve anything')
    expect(screen.getByTestId('rule-editor')).toHaveTextContent('not answered')
  })

  it('prefills the text from the question and the chosen option, and follows the choice until you edit it', async () => {
    const user = userEvent.setup()
    render(card())
    await user.click(screen.getByTestId('record-rule'))
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('')
    await user.click(screen.getByTestId('record-choice-deny'))
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Run the migration”, my answer is: Deny.')
    await user.click(screen.getByTestId('record-choice-allow'))
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Run the migration”, my answer is: Allow once.')
    await user.type(screen.getByTestId('rule-editor-text'), ' Always run it from the repo root.')
    await user.click(screen.getByTestId('record-choice-deny'))
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Run the migration”, my answer is: Allow once. Always run it from the repo root.')
  })

  it('saves an active rule for the session’s project with the answer as its source, and answers nothing', async () => {
    const user = userEvent.setup()
    render(card())
    await user.click(screen.getByTestId('record-rule'))
    expect(screen.getByTestId('rule-editor-scope')).toHaveValue('web')
    await user.click(screen.getByTestId('record-choice-allow'))
    await user.type(screen.getByTestId('rule-editor-text'), ' Run it from the repo root.')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(daemon.calls('rule.add')).toHaveLength(1)
    expect(daemon.calls('rule.add')[0]).toMatchObject({ text: 'When asked “Run the migration”, my answer is: Allow once. Run it from the repo root.', project: 'web', status: 'active', source: { kind: 'answer', session_id: 's1', request_id: 'r1' } })
    expect(onRespond).not.toHaveBeenCalled()
    expect(rpc.methods().filter((m) => m.startsWith('permission.'))).toEqual([])
    // the request card is still waiting for its answer
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-state', 'pending')
    expect(screen.getByTestId('permission-option-allow')).toBeEnabled()
  })

  it('never saves without you pressing save: opening and cancelling creates nothing', async () => {
    const user = userEvent.setup()
    render(card())
    await user.click(screen.getByTestId('record-rule'))
    await user.click(screen.getByTestId('rule-editor-cancel'))
    expect(daemon.calls('rule.add')).toHaveLength(0)
    expect(screen.queryByTestId('rule-editor')).toBeNull()
  })

  it('shows a refusal (the rules are full) and the request is still unanswered', async () => {
    const user = userEvent.setup()
    render(card())
    await user.click(screen.getByTestId('record-rule'))
    await user.type(screen.getByTestId('rule-editor-text'), 'something')
    daemon.failNext('rule.add', (await import('@/test/fakeRulesDaemon')).ruleError('invalid', { field: 'text', reason: 'over_budget', bytes: 4200, cap: 4096 }))
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(screen.getByTestId('rule-editor-error')).toHaveTextContent(/4200 bytes/)
    expect(onRespond).not.toHaveBeenCalled()
  })

  it('has an editor that counts as an open dialog, which is what keeps ⌘⇧A / ⌘⇧D and J / K from doing anything', async () => {
    const user = userEvent.setup()
    render(card())
    expect(overlayOpen()).toBe(false)
    await user.click(screen.getByTestId('record-rule'))
    expect(overlayOpen()).toBe(true)
  })

  it('does not touch the answer buttons: they behave exactly as without it (and are not enabled by it when locked)', async () => {
    const user = userEvent.setup()
    render(<PermissionCard block={tool} deviceLabel={() => 'you'} onRespond={onRespond} busy recordAsRule={{ nodeId: N, sessionId: 's1' }} />)
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    expect(screen.getByTestId('record-rule')).toBeEnabled()
    await user.click(screen.getByTestId('record-rule'))
    await user.click(screen.getByTestId('rule-editor-cancel'))
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    expect(onRespond).not.toHaveBeenCalled()
  })
})

describe('the editor for a question', () => {
  it('prefills with the question and the answers already chosen in the card; the agent’s question and choices are shown apart', async () => {
    const user = userEvent.setup()
    render(card(question))
    await user.click(within(screen.getByTestId('permission-questions')).getByLabelText('pnpm'))
    await user.click(screen.getByTestId('record-rule'))
    expect(screen.getByTestId('record-agent')).toHaveTextContent('Which package manager should I use?')
    expect(screen.getByTestId('record-agent')).toHaveTextContent('- npm')
    expect(screen.getByTestId('record-answers')).toHaveTextContent('pnpm')
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Which package manager should I use?”, my answer is: pnpm.')
    await user.type(screen.getByTestId('rule-editor-text'), ' Always.')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(daemon.calls('rule.add')[0]).toMatchObject({ source: { kind: 'answer', session_id: 's1', request_id: 'q1' } })
    expect(onRespond).not.toHaveBeenCalled()
  })

  it('with nothing chosen yet it says so and starts empty', async () => {
    const user = userEvent.setup()
    render(card(question))
    await user.click(screen.getByTestId('record-rule'))
    expect(screen.getByTestId('record-no-answer')).toBeInTheDocument()
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('')
  })
})

describe('the agent text is only ever shown as text', () => {
  it('markup and HTML in what the agent wrote are not interpreted', async () => {
    const user = userEvent.setup()
    const nasty: PermissionBlock = { ...tool, title: '<img src=x onerror=alert(1)> **bold**', detail: { tool: { tool: 'Bash', category: 'shell', input: { command: '<b>cmd</b>' }, cwd: '/w', detail: { command: '<b>cmd</b>' } } } }
    render(card(nasty))
    await user.click(screen.getByTestId('record-rule'))
    const agent = screen.getByTestId('record-agent')
    expect(agent.querySelector('img, b, strong')).toBeNull()
    expect(agent).toHaveTextContent('<img src=x onerror=alert(1)> **bold**')
  })
})

it('keeps the rules store untouched until saving', async () => {
  const user = userEvent.setup()
  render(card())
  await user.click(screen.getByTestId('record-rule'))
  expect(useRules.getState().nodes[N]).toBeUndefined()
})

describe('the unedited prefill cannot be saved', () => {
  it('saving is off while the text is still exactly what was prefilled, with a hint, and on once it is changed', async () => {
    const user = userEvent.setup()
    render(card())
    await user.click(screen.getByTestId('record-rule'))
    await user.click(screen.getByTestId('record-choice-allow'))
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Run the migration”, my answer is: Allow once.')
    expect(screen.getByTestId('rule-editor-save')).toBeDisabled()
    expect(screen.getByTestId('rule-editor-edit-first')).toBeInTheDocument()
    await user.type(screen.getByTestId('rule-editor-text'), ' x')
    expect(screen.getByTestId('rule-editor-save')).toBeEnabled()
    expect(screen.queryByTestId('rule-editor-edit-first')).toBeNull()
    // back to the prefill: off again
    await user.type(screen.getByTestId('rule-editor-text'), '{Backspace}{Backspace}')
    expect(screen.getByTestId('rule-editor-save')).toBeDisabled()
  })

  it('following a change of choice, the new prefill is also unsaved-as-is', async () => {
    const user = userEvent.setup()
    render(card())
    await user.click(screen.getByTestId('record-rule'))
    await user.click(screen.getByTestId('record-choice-allow'))
    await user.click(screen.getByTestId('record-choice-deny'))
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Run the migration”, my answer is: Deny.')
    expect(screen.getByTestId('rule-editor-save')).toBeDisabled()
  })

  it('Enter in a radio button or the scope select does not save; only the Save button does', async () => {
    const user = userEvent.setup()
    render(card())
    await user.click(screen.getByTestId('record-rule'))
    await user.click(screen.getByTestId('record-choice-allow'))
    await user.type(screen.getByTestId('rule-editor-text'), ' Edited.')
    screen.getByTestId('record-choice-allow').focus()
    await user.keyboard('{Enter}')
    screen.getByTestId('rule-editor-scope').focus()
    await user.keyboard('{Enter}')
    await flush()
    expect(daemon.calls('rule.add')).toHaveLength(0)
    screen.getByTestId('rule-editor-save').focus()
    await user.keyboard('{Enter}')
    await flush()
    expect(daemon.calls('rule.add')).toHaveLength(1)
  })
})

describe('what the agent wrote is cleaned before it goes into the prefill', () => {
  const dirty = `Run\u202Etxet\u200B the\nmigration\t\u0007 now`

  it('control characters become spaces, invisible characters go, and the prefill can be saved without an invisible-characters error', async () => {
    const user = userEvent.setup()
    const block: PermissionBlock = { ...tool, title: dirty }
    render(card(block))
    await user.click(screen.getByTestId('record-rule'))
    await user.click(screen.getByTestId('record-choice-allow'))
    const value = (screen.getByTestId('rule-editor-text') as HTMLTextAreaElement).value
    expect(value).toBe('When asked “Runtxet the migration now”, my answer is: Allow once.')
    await user.type(screen.getByTestId('rule-editor-text'), ' Mine.')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(screen.queryByTestId('rule-editor-error')).toBeNull()
    expect(daemon.calls('rule.add')).toHaveLength(1)
  })

  it('option labels and the question’s choice labels are cleaned too, and a long agent title is clipped', async () => {
    const user = userEvent.setup()
    const opts: PermissionBlock = { ...tool, title: 'x'.repeat(500), options: [{ id: 'allow', label: 'Al\u202Elow\nonce', kind: 'allow_once' }] }
    const { unmount } = render(card(opts))
    await user.click(screen.getByTestId('record-rule'))
    await user.click(screen.getByTestId('record-choice-allow'))
    const value = (screen.getByTestId('rule-editor-text') as HTMLTextAreaElement).value
    expect(value).toContain('my answer is: Allow once.')
    expect([...value].length).toBeLessThan(220)
    unmount()
    const q: PermissionBlock = { ...question, detail: { questions: [{ id: 'pm', text: 'Which\u200B one?', choices: [{ id: 'a', label: 'p\u202Enpm\nx' }] }] } }
    render(card(q))
    await user.click(within(screen.getByTestId('permission-questions')).getByRole('radio'))
    await user.click(screen.getByTestId('record-rule'))
    expect((screen.getByTestId('rule-editor-text') as HTMLTextAreaElement).value).toBe('When asked “Which one?”, my answer is: pnpm x.')
  })
})
