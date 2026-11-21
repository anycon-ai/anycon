// 「待处理」的「稍后」段里的待确认规则（spec/web.md § 4.11、§ 4.14）：一条规则一行；行上没有任何动作，确认只在显示着全文的详情里；
// 从不替用户选中 / 展开；不进数字、角标、标题；刚换到这一条的头一小会儿动作锁着；别处处理掉的不替用户挑下一条。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { ARM_DELAY_MS } from '@/features/inbox/arming'
import { inboxNowCount } from '@/features/inbox/derive'
import { getInboxSnapshot, inboxNowCountFromStores } from '@/features/inbox/model'
import { useInflight } from '@/features/session/inflight'
import { cancelRuleRefreshes, handleRuleChanged, resetRulesMemory, syncRules } from '@/features/rules/service'
import { nodeEntry, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeRulesDaemon, ruleItem } from '@/test/fakeRulesDaemon'
import { mockRouter } from '@/test/routerMock'
import { resetViewport, setViewport } from '@/test/viewport'
import { setRpc } from '@/transport/instance'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type PendingPermission } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/inbox' }))

import { InboxPage } from './InboxPage'

const A = 'a'.repeat(32)
const info = (rules: boolean): NodeInfo => ({ capabilities: { rules } }) as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeRulesDaemon
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 10; i++) await new Promise((r) => setTimeout(r, 5))
  })
}
const proposed = (id: string, text = `proposed ${id}`, created = 2_000) => ruleItem({ id, text, status: 'pending', source: { kind: 'agent', session_id: 's1' }, created_ms: created })
const laterRows = (): HTMLElement[] => within(screen.getByTestId('inbox-section-later')).queryAllByTestId('inbox-row')
const arm = (): void => {
  act(() => void vi.advanceTimersByTime(ARM_DELAY_MS + 10))
}
const request = (id: string): PendingPermission => ({
  request_id: id,
  session_id: `s-${id}`,
  node_id: A,
  kind: 'tool',
  title: `Run ${id}`,
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: `echo ${id}` }, cwd: '/w', detail: { command: `echo ${id}` } } },
  options: [{ id: 'allow', label: 'Allow once', kind: 'allow_once' }],
  created_ms: 0,
  created_at_ms: Date.now() - 1000,
  expires_at_ms: null,
} as PendingPermission)

async function load(...rules: Array<ReturnType<typeof ruleItem>>): Promise<void> {
  daemon.seed(...rules)
  await syncRules(A)
  render(<InboxPage />)
  await flush()
}

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true })
  rpc = new FakeRpc()
  daemon = new FakeRulesDaemon()
  daemon.install(rpc)
  daemon.onChange = (id, rule) => handleRuleChanged(A, rule ? { id, rule, deleted: false } : { id, deleted: true })
  setRpc(rpc)
  resetRulesMemory()
  resetViewport()
  useInflight.setState({ ids: new Set(), handled: new Set() })
  usePrefs.setState({ sidebarNodes: null })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(true), conn: 'ready' }) }, order: [A], loaded: true })
  useUi.setState({ toasts: [] })
})
afterEach(() => {
  cancelRuleRefreshes()
  setRpc(null)
  vi.useRealTimers()
  vi.restoreAllMocks()
})

describe('pending rules in Later', () => {
  it('one row per rule, labelled as a rule to review, with a count on the section header; only pending rules are there', async () => {
    await load(proposed('r1'), proposed('r2'), ruleItem({ id: 'on' }), ruleItem({ id: 'off', status: 'disabled' }), ruleItem({ id: 'no', status: 'rejected' }), ruleItem({ id: 'weird', status: 'expired' as never }))
    expect(laterRows()).toHaveLength(2)
    expect(screen.getByTestId('inbox-count-later')).toHaveTextContent('2')
    expect(within(laterRows()[0]!).getByText('Rule to review')).toBeInTheDocument()
    expect(laterRows()[0]).toHaveAttribute('data-kind', 'rule')
  })

  it('the row shows only the first line of the text and has no action buttons at all', async () => {
    await load(proposed('r1', 'First line of the rule\nsecond line that is only in the details'))
    const row = laterRows()[0]!
    expect(row).toHaveTextContent('First line of the rule second line')
    expect(within(row).queryAllByRole('button')).toHaveLength(1) // just the row's own select button
    for (const id of ['inbox-accept', 'inbox-dismiss', 'inbox-rule-confirm', 'inbox-rule-reject']) expect(within(row).queryByTestId(id)).toBeNull()
  })

  it('is never selected or expanded on its own, on desktop or on a phone, even when it is the only thing there', async () => {
    await load(proposed('r1'))
    expect(screen.queryByTestId('inbox-detail')).toBeNull()
    expect(laterRows()[0]).not.toHaveAttribute('data-selected', 'true')
    // phone
    document.body.innerHTML = ''
    setViewport('phone')
    render(<InboxPage />)
    await flush()
    expect(screen.queryByTestId('inbox-detail')).toBeNull()
    expect(within(screen.getByTestId('inbox-section-later')).getByTestId('inbox-row-main')).toHaveAttribute('aria-expanded', 'false')
  })

  it('does not take the default selection from a request that is waiting: the request is the one selected', async () => {
    useSessions.setState({ list: {}, pending: { q1: request('q1') }, open: {} })
    await load(proposed('r1'))
    expect(screen.getByTestId('inbox-detail')).toHaveAttribute('data-kind', 'permission')
  })

  it('does not count in the number on the nav, the badge or the title', async () => {
    daemon.seed(proposed('r1'), proposed('r2'))
    await syncRules(A)
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(0)
    expect(inboxNowCountFromStores()).toBe(0)
    render(<InboxPage />)
    await flush()
    expect(screen.getByTestId('inbox-count-later')).toHaveTextContent('2')
    expect(document.title).not.toMatch(/\(\d+\)/)
  })

  it('stays out of the picture on a node that does not support rules', async () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(false), conn: 'ready' }) }, order: [A], loaded: true })
    await load(proposed('r1'))
    expect(screen.queryByTestId('inbox-section-later')).toBeNull()
  })
})

describe('the details of a rule', () => {
  it('select a row: the whole text, scope, source and a note are on screen before anything can be confirmed', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    const long = 'Prefer small commits.\n' + 'Explain why in the message. '.repeat(10) + '\n<b>not html</b>'
    await load(proposed('r1', long))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    const detail = screen.getByTestId('inbox-detail')
    expect(detail).toHaveAttribute('data-kind', 'rule')
    expect(within(detail).getByTestId('inbox-rule-text').textContent).toBe(long)
    expect(within(detail).getByTestId('rule-source')).toHaveTextContent('Proposed by an agent')
    expect(within(detail).getByTestId('rule-scope')).toBeInTheDocument()
    expect(detail).toHaveTextContent(/exactly as written/)
    expect(detail.querySelector('b')).toBeNull()
  })

  it('the buttons are locked for a moment after switching to it, and only then confirm with the version it was read at', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    await load(proposed('r1'))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    expect(screen.getByTestId('inbox-rule-confirm')).toBeDisabled()
    expect(screen.getByTestId('inbox-rule-reject')).toBeDisabled()
    expect(screen.getByTestId('inbox-rule-edit')).toBeDisabled()
    arm()
    expect(screen.getByTestId('inbox-rule-confirm')).toBeEnabled()
    await user.click(screen.getByTestId('inbox-rule-confirm'))
    await flush()
    expect(daemon.calls('rule.update')[0]).toEqual({ id: 'r1', status: 'active', if_updated_ms: 1_000 })
    await waitFor(() => expect(screen.queryByTestId('inbox-section-later')).toBeNull())
  })

  it('reject keeps the rule on the node but takes it out of the list', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    await load(proposed('r1'))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    arm()
    await user.click(screen.getByTestId('inbox-rule-reject'))
    await flush()
    expect(daemon.rules.get('r1')!.status).toBe('rejected')
  })

  it('edit then confirm opens the editor with the text and saves both in one request', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    await load(proposed('r1', 'draft wording'))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    arm()
    await user.click(screen.getByTestId('inbox-rule-edit'))
    const text = screen.getByTestId('rule-editor-text')
    expect(text).toHaveValue('draft wording')
    await user.clear(text)
    await user.type(text, 'final wording')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(daemon.calls('rule.update')[0]).toEqual({ id: 'r1', text: 'final wording', status: 'active', if_updated_ms: 1_000 })
  })

  it('after the next rule appears the selection moves to the rule that is left, not into the requests above', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    await load(proposed('r1', 'one', 1_000), proposed('r2', 'two', 2_000))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    arm()
    await user.click(screen.getByTestId('inbox-rule-confirm'))
    await flush()
    await waitFor(() => expect(laterRows()).toHaveLength(1))
    expect(screen.queryByTestId('inbox-ghost')).toBeNull()
    expect(laterRows()[0]).toHaveAttribute('data-selected', 'true')
    expect(screen.getByTestId('inbox-rule-text')).toHaveTextContent('two')
  })

  it('a rule confirmed on another device leaves a placeholder instead of selecting the next one', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    await load(proposed('r1', 'one', 1_000), proposed('r2', 'two', 2_000))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    arm()
    daemon.changeElsewhere('r1', { status: 'active' })
    await flush()
    expect(screen.getByTestId('inbox-ghost')).toBeInTheDocument()
    expect(laterRows().every((r) => r.getAttribute('data-selected') !== 'true')).toBe(true)
  })

  it('on a node that goes offline the actions are disabled and the cached note says why', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    await load(proposed('r1'))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    arm()
    act(() => useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(true), conn: 'peer_offline' }) } }))
    expect(screen.getByTestId('inbox-rule-confirm')).toBeDisabled()
    expect(screen.getByTestId('inbox-cached-note')).toBeInTheDocument()
  })

  it('a rule that came from a session links back to it when the session is known', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    useSessions.setState({ list: { s1: { ...summary({ id: 's1', title: 'the proposer' }), node_id: A, cached_seq: 0 } as never }, pending: {}, open: {} })
    await load(proposed('r1'))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    expect(screen.getByTestId('rule-source-session')).toHaveAttribute('href', expect.stringContaining('/s/s1'))
  })
})
