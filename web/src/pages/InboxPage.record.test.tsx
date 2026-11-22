// 「记成规则」在「待处理」的请求详情里（docs/spec/daemon.md § 23.6）：有这个按钮；打开编辑框不答复请求、不改答复按钮的解锁；
// 编辑框开着时 ⌘⇧A / ⌘⇧D 什么都不做（也不会经它答复）；保存之后请求仍在等。
import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { ARM_DELAY_MS } from '@/features/inbox/arming'
import { useInflight } from '@/features/session/inflight'
import { cancelRuleRefreshes, resetRulesMemory } from '@/features/rules/service'
import { useFollowUp } from '@/features/rules/followUp'
import { FollowUpRuleHost } from '@/features/rules/ui/AlwaysDoThis'
import { nodeEntry, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeRulesDaemon } from '@/test/fakeRulesDaemon'
import { mockRouter } from '@/test/routerMock'
import { resetViewport } from '@/test/viewport'
import { setRpc } from '@/transport/instance'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type PendingPermission } from '@/stores/sessions'

const h = vi.hoisted(() => ({ respond: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/inbox' }))
vi.mock('@/features/session/actions', async (orig) => ({ ...(await orig<object>()), respondPermission: h.respond }))

import { InboxPage } from './InboxPage'

const A = 'a'.repeat(32)
const info = { capabilities: { rules: true } } as unknown as NodeInfo
const req = (id: string): PendingPermission => ({
  request_id: id,
  session_id: 's1',
  node_id: A,
  kind: 'tool',
  title: `Run ${id}`,
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: `echo ${id}` }, cwd: '/w', detail: { command: `echo ${id}` } } },
  options: [
    { id: 'allow', label: 'Allow once', kind: 'allow_once' },
    { id: 'deny', label: 'Deny once', kind: 'reject_once' },
  ],
  created_at_ms: Date.now() - 1000,
  expires_at_ms: null,
} as PendingPermission)

let daemon: FakeRulesDaemon
const arm = (): void => {
  act(() => void vi.advanceTimersByTime(ARM_DELAY_MS + 10))
}

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true })
  const rpc = new FakeRpc()
  daemon = new FakeRulesDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  resetRulesMemory()
  resetViewport()
  h.respond.mockReset().mockResolvedValue('answered')
  useInflight.setState({ ids: new Set(), handled: new Set() })
  usePrefs.setState({ sidebarNodes: null })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info, conn: 'ready' }) }, order: [A], loaded: true })
  useSessions.setState({ list: { s1: { ...summary({ id: 's1', title: 'work' }), node_id: A, cached_seq: 0 } as never }, pending: { r1: req('r1') }, open: {} })
})
afterEach(() => {
  cancelRuleRefreshes()
  setRpc(null)
  vi.useRealTimers()
  vi.restoreAllMocks()
})

describe('Record as a rule in the Inbox', () => {
  it('is offered in the request details and is usable while the answer buttons are still locked, without unlocking them', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    render(<InboxPage />)
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    await user.click(screen.getByTestId('record-rule'))
    await user.click(screen.getByTestId('rule-editor-cancel'))
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    arm()
    expect(screen.getByTestId('permission-option-allow')).toBeEnabled()
    expect(h.respond).not.toHaveBeenCalled()
  })

  it('⌘⇧A and ⌘⇧D do nothing while its editor is open, and saving the rule leaves the request waiting', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({ top: 10, bottom: 60, left: 0, right: 10, width: 10, height: 50, x: 0, y: 10, toJSON: () => ({}) })
    render(<InboxPage />)
    arm()
    await user.click(screen.getByTestId('record-rule'))
    for (const key of ['A', 'D']) act(() => void document.body.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, metaKey: true, shiftKey: true, key })))
    expect(h.respond).not.toHaveBeenCalled()
    await user.type(screen.getByTestId('rule-editor-text'), 'Ask me before running migrations.')
    await user.click(screen.getByTestId('rule-editor-save'))
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20))
    })
    expect(daemon.calls('rule.add')[0]).toMatchObject({ source: { kind: 'answer', session_id: 's1', request_id: 'r1' }, status: 'active' })
    expect(h.respond).not.toHaveBeenCalled()
    expect(useSessions.getState().pending.r1).toBeDefined()
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-state', 'pending')
  })
})

describe('Always do this in the Inbox', () => {
  const askQuestion = (): void => {
    const q = {
      ...req('r1'),
      kind: 'question',
      title: 'Which package manager?',
      detail: { questions: [{ id: 'pm', text: 'Which package manager should I use?', choices: [{ id: 'pnpm', label: 'pnpm' }, { id: 'npm', label: 'npm' }] }] },
      options: [{ id: 'submit', label: 'Submit answers', kind: 'allow_once' }],
    } as unknown as PendingPermission
    useSessions.setState({ pending: { r1: q } })
    useFollowUp.getState().close()
  }

  it('ticking it while the answer buttons are locked does not unlock them, and ⌘⇧A / ⌘⇧D still answer nothing for a question', async () => {
    askQuestion()
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({ top: 10, bottom: 60, left: 0, right: 10, width: 10, height: 50, x: 0, y: 10, toJSON: () => ({}) })
    render(<InboxPage />)
    await user.click(screen.getByLabelText('pnpm'))
    await user.click(screen.getByTestId('always-do-this-box'))
    expect(screen.getByTestId('permission-option-submit')).toBeDisabled()
    await user.click(screen.getByTestId('permission-option-submit'))
    for (const key of ['A', 'D']) act(() => void document.body.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, metaKey: true, shiftKey: true, key })))
    expect(h.respond).not.toHaveBeenCalled()
    arm()
    expect(screen.getByTestId('permission-option-submit')).toBeEnabled()
    for (const key of ['A', 'D']) act(() => void document.body.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, metaKey: true, shiftKey: true, key })))
    expect(h.respond).not.toHaveBeenCalled()
    expect(screen.queryByTestId('rule-editor')).toBeNull()
  })

  it('after the unlock, a ticked answer sends the same request and then opens the editor, even though the request leaves the list', async () => {
    askQuestion()
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    render(
      <>
        <InboxPage />
        <FollowUpRuleHost />
      </>,
    )
    arm()
    await user.click(screen.getByLabelText('pnpm'))
    await user.click(screen.getByTestId('always-do-this-box'))
    await user.click(screen.getByTestId('permission-option-submit'))
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20))
    })
    expect(h.respond).toHaveBeenCalledTimes(1)
    expect(h.respond.mock.calls[0]![1]).toEqual({ request_id: 'r1', answers: { pm: 'pnpm' }, option_id: 'submit' })
    expect(screen.getByTestId('rule-editor')).toBeInTheDocument()
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Which package manager should I use?”, my answer is: pnpm.')
    expect(screen.getByTestId('rule-editor-save')).toBeDisabled()
  })
})
