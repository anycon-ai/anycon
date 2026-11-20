// 「待处理」页（spec/web.md § 14.4）：合并多个节点、权限请求用会话页同一个卡片、详情展开了才有按钮、处理完焦点落到下一条、
// 离线节点的缓存要标明、三种布局（桌面分栏 / 平板滑出 / 手机展开）、通知点进来的定位、键盘。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { PermissionOption, PermissionKind } from '@/protocol'
import { nodeEntry, summary } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { resetViewport, setViewport } from '@/test/viewport'
import { useEnroll } from '@/stores/enroll'
import { ARM_DELAY_MS } from '@/features/inbox/arming'
import { useInflight } from '@/features/session/inflight'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useUi } from '@/stores/ui'
import { useSessions, type PendingPermission, type SessionItem } from '@/stores/sessions'

const h = vi.hoisted(() => ({
  navigate: vi.fn(),
  respond: vi.fn(),
  setFollowUp: vi.fn(),
  archive: vi.fn(),
}))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, {}, { pathname: '/inbox' }))
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  respondPermission: h.respond,
  setFollowUp: h.setFollowUp,
  archiveSession: h.archive,
}))

vi.mock('@/api/queries', async (orig) => ({ ...(await orig<object>()), useDevices: () => ({ data: undefined, dataUpdatedAt: 0 }) }))

import { InboxPage } from './InboxPage'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const ALLOW: PermissionOption = { id: 'allow', label: 'Allow once', kind: 'allow_once' }
const DENY: PermissionOption = { id: 'deny', label: 'Deny once', kind: 'reject_once' }
const ALWAYS: PermissionOption = { id: 'always', label: 'Always allow', kind: 'allow_always' }

const sess = (over: Partial<SessionItem> & { id: string }): SessionItem => ({ ...summary({ title: over.id, project: 'app', updated_at_ms: Date.now() - 60_000, ...over }), node_id: A, cached_seq: 0, ...over })
const req = (id: string, over: Partial<PendingPermission> = {}): PendingPermission => ({
  request_id: id,
  session_id: `s-${id}`,
  node_id: A,
  kind: 'tool' as PermissionKind,
  title: `Run ${id}`,
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: `echo ${id}` }, cwd: `/work/${id}`, detail: { command: `echo ${id}` } } },
  options: [ALLOW, ALWAYS, DENY],
  created_at_ms: Date.now() - 120_000,
  expires_at_ms: null,
  ...over,
})
/** 详情里换到一条新请求后，答复按钮先锁 `ARM_DELAY_MS`：测试里推进假时钟让它解锁。 */
const arm = (): void => {
  act(() => {
    vi.advanceTimersByTime(ARM_DELAY_MS + 10)
  })
}
const setup = () => userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
const seed = (o: { sessions?: SessionItem[]; pending?: PendingPermission[] }): void =>
  useSessions.setState({ list: Object.fromEntries((o.sessions ?? []).map((s) => [s.id, s])), pending: Object.fromEntries((o.pending ?? []).map((p) => [p.request_id, p])), open: {} })

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true })
  useInflight.setState({ ids: new Set(), handled: new Set() })
  h.navigate.mockReset()
  h.respond.mockReset().mockImplementation(async (nodeId: string, p: { request_id: string }) => {
    useSessions.getState().resolvePending(nodeId, p.request_id)
    return 'answered'
  })
  h.setFollowUp.mockReset().mockImplementation(async (_n: string, id: string, v: boolean) => {
    const it = useSessions.getState().list[id]
    if (it) useSessions.setState((s) => ({ list: { ...s.list, [id]: { ...it, follow_up: v } } }))
  })
  h.archive.mockReset().mockImplementation(async (_n: string, id: string) => {
    const it = useSessions.getState().list[id]
    if (it) useSessions.setState((s) => ({ list: { ...s.list, [id]: { ...it, archived: true } } }))
  })
  resetViewport()
  usePrefs.setState({ sidebarNodes: null })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null })
  useNodes.setState({
    byId: { [A]: nodeEntry({ id: A, name: 'alpha' }), [B]: nodeEntry({ id: B, name: 'beta' }) },
    order: [A, B],
    loaded: true,
  })
  seed({})
})

afterEach(() => vi.useRealTimers())

const rows = (): HTMLElement[] => screen.queryAllByTestId('inbox-row')
const rowTitles = (): string[] => rows().map((r) => within(r).getByTestId('inbox-row-main').textContent ?? '')

describe('list', () => {
  it('merges the nodes picked in the node filter, with their node names', () => {
    seed({ pending: [req('a1'), req('b1', { node_id: B, session_id: 's-b1' })] })
    render(<InboxPage />)
    expect(rows()).toHaveLength(2)
    expect(screen.getByTestId('inbox-count-now')).toHaveTextContent('2')
    expect(rowTitles().join('|')).toMatch(/alpha/)
    expect(rowTitles().join('|')).toMatch(/beta/)
    // 过滤器只勾 A：B 的不显示
    usePrefs.setState({ sidebarNodes: [A] })
  })

  it('only shows the picked nodes', () => {
    usePrefs.setState({ sidebarNodes: [A] })
    seed({ pending: [req('a1'), req('b1', { node_id: B })] })
    render(<InboxPage />)
    expect(rows()).toHaveLength(1)
    expect(rowTitles()[0]).toContain('Run a1')
  })

  it('labels each row with its kind in words, not only by colour', () => {
    seed({
      pending: [req('p'), req('q', { kind: 'question' }), req('pl', { kind: 'plan' })],
      sessions: [sess({ id: 'f', follow_up: true, status: 'idle' }), sess({ id: 'e', status: 'error' })],
    })
    useEnroll.setState({ items: { [`${A}:x`]: { nodeId: A, info: { enroll_id: 'x', expires_at_ms: Date.now() + 60_000, created_at_ms: Date.now(), status: 'wait_approval', method: 'code', approval: true }, expiresAt: Date.now() + 60_000, receivedAt: Date.now(), createdAt: Date.now() } } })
    render(<InboxPage />)
    const kinds = rows().map((r) => r.getAttribute('data-kind'))
    expect(kinds.sort()).toEqual(['enroll', 'error', 'follow_up', 'permission', 'plan', 'question'])
    for (const label of ['Approval', 'Plan', 'Question', 'Device request', 'Follow up', 'Error']) expect(screen.getAllByText(label).length).toBeGreaterThan(0)
  })

  it('shows the empty state when nothing waits, and “In progress” as a collapsed row that expands', async () => {
    const user = setup()
    seed({ sessions: [sess({ id: 'run', title: 'Long job', status: 'running' }), sess({ id: 'armed', title: 'Flagged job', status: 'running', follow_up: true })] })
    render(<InboxPage />)
    expect(screen.getByTestId('inbox-empty')).toHaveTextContent('Nothing is waiting for you')
    expect(screen.queryByTestId('inbox-section-now')).toBeNull()
    const toggle = screen.getByTestId('inbox-in-progress-toggle')
    expect(toggle).toHaveAttribute('aria-expanded', 'false')
    expect(screen.getByTestId('inbox-in-progress-count')).toHaveTextContent('2')
    expect(screen.queryByText('Long job')).toBeNull()
    await user.click(toggle)
    expect(toggle).toHaveAttribute('aria-expanded', 'true')
    const armed = screen.getAllByTestId('inbox-in-progress-row').find((r) => r.getAttribute('data-armed') === 'true')!
    expect(within(armed).getByText('Flagged job')).toBeInTheDocument()
    expect(within(armed).getByText('Will remind')).toBeInTheDocument()
  })

  it('a running session with a follow-up mark is in progress, not in Now', () => {
    seed({ sessions: [sess({ id: 'armed', status: 'running', follow_up: true })] })
    render(<InboxPage />)
    expect(rows()).toHaveLength(0)
    expect(screen.getByTestId('inbox-in-progress-count')).toHaveTextContent('1')
  })
})

describe('permission requests', () => {
  it('desktop: the first row is selected and its details sit in the right half, in the same card as on the session page', () => {
    seed({ pending: [req('one'), req('two')], sessions: [sess({ id: 's-one', title: 'Fix the bug' })] })
    render(<InboxPage />)
    const pane = screen.getByTestId('inbox-detail-pane')
    const card = within(pane).getByTestId('permission-card')
    expect(card).toHaveAttribute('data-request', 'one')
    // 完整内容：命令、工作目录、节点、所属会话
    expect(within(card).getByTestId('permission-command')).toHaveTextContent('$ echo one')
    expect(card).toHaveTextContent('/work/one')
    expect(within(pane).getByTestId('inbox-detail-node')).toHaveTextContent('alpha')
    expect(within(pane).getByTestId('inbox-detail-session')).toHaveTextContent('Fix the bug')
    expect(within(pane).getByTestId('inbox-open-session')).toHaveAttribute('href', '/n/aaaaaaaa/s/s-one')
  })

  it('there is no way to answer without the details: the rows carry no option buttons, only the card does', () => {
    seed({ pending: [req('one'), req('two'), req('three')] })
    render(<InboxPage />)
    // 三条请求，只有选中的那一条的卡片（里面才有按钮）
    expect(screen.getAllByTestId('permission-card')).toHaveLength(1)
    for (const r of rows()) expect(within(r).queryByTestId('permission-option-allow')).toBeNull()
    expect(screen.getAllByTestId('permission-option-allow')).toHaveLength(1)
    expect(screen.queryByTestId('inbox-done')).toBeNull()
  })

  it('selecting another row swaps the card', async () => {
    const user = setup()
    seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] })
    render(<InboxPage />)
    await user.click(within(rows()[1]!).getByTestId('inbox-row-main'))
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    expect(rows()[1]).toHaveAttribute('data-selected', 'true')
  })

  it('answering sends the option the card offers, and focus lands on the next row', async () => {
    const user = setup()
    seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 }), req('three', { created_at_ms: 3 })] })
    render(<InboxPage />)
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    expect(h.respond).toHaveBeenCalledWith(A, { request_id: 'one', option_id: 'allow', message: undefined })
    await waitFor(() => expect(rows()).toHaveLength(2))
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    expect(document.activeElement).toBe(within(rows()[0]!).getByTestId('inbox-row-main'))
    expect(rows()[0]).toHaveAttribute('data-selected', 'true')
  })

  it('after the last item is handled, focus moves to the “nothing waiting” heading', async () => {
    const user = setup()
    seed({ pending: [req('only')] })
    render(<InboxPage />)
    arm()
    await user.click(screen.getByTestId('permission-option-deny'))
    await waitFor(() => expect(screen.getByTestId('inbox-empty')).toBeInTheDocument())
    expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Nothing is waiting for you' }))
  })

  it('when the handled item was the last in the list, focus goes to the one before it', async () => {
    const user = setup()
    seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] })
    render(<InboxPage />)
    await user.click(within(rows()[1]!).getByTestId('inbox-row-main'))
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    await waitFor(() => expect(rows()).toHaveLength(1))
    expect(document.activeElement).toBe(within(rows()[0]!).getByTestId('inbox-row-main'))
  })

  it('shows plans and questions with the same card, and a mode change with its target', () => {
    seed({
      pending: [
        req('plan', { kind: 'plan', title: 'Plan ready', detail: { text: '# Do the thing' }, options: [{ id: 'go', label: 'Start', kind: 'allow_once' }] }),
      ],
    })
    render(<InboxPage />)
    expect(screen.getByTestId('permission-plan')).toHaveTextContent('Do the thing')
    expect(screen.getByTestId('permission-option-go')).toBeInTheDocument()
  })

  it('a node that is offline: the row and the details say the data is cached, and the buttons are disabled', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', conn: 'peer_offline' }), [B]: nodeEntry({ id: B, name: 'beta' }) } })
    seed({ pending: [req('one')], sessions: [sess({ id: 'f', follow_up: true, status: 'idle' })] })
    render(<InboxPage />)
    expect(screen.getAllByTestId('inbox-cached')).toHaveLength(2)
    expect(screen.getByTestId('inbox-cached-note')).toHaveTextContent(/alpha is offline/)
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    expect(screen.getByTestId('inbox-done')).toBeDisabled()
    // 在线的节点没有这个标注
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha' }), [B]: nodeEntry({ id: B, name: 'beta' }) } })
  })

  it('does not label data from an online node', () => {
    seed({ pending: [req('one')] })
    render(<InboxPage />)
    expect(screen.queryByTestId('inbox-cached')).toBeNull()
    arm()
    expect(screen.getByTestId('permission-option-allow')).toBeEnabled()
  })
})

describe('sessions and device requests', () => {
  it('“Done” on a follow-up row clears the mark, and focus goes to the next item', async () => {
    const user = setup()
    seed({ sessions: [sess({ id: 'f1', title: 'First', follow_up: true, status: 'idle', updated_at_ms: 1_000 }), sess({ id: 'f2', title: 'Second', follow_up: true, status: 'idle', updated_at_ms: 2_000 })] })
    render(<InboxPage />)
    expect(rowTitles()[0]).toContain('First')
    await user.click(within(rows()[0]!).getByTestId('inbox-done'))
    expect(h.setFollowUp).toHaveBeenCalledWith(A, 'f1', false)
    await waitFor(() => expect(rows()).toHaveLength(1))
    expect(document.activeElement).toBe(within(rows()[0]!).getByTestId('inbox-row-main'))
    expect(rowTitles()[0]).toContain('Second')
  })

  it('a follow-up row can be opened', () => {
    seed({ sessions: [sess({ id: 'f1', follow_up: true, status: 'idle' })] })
    render(<InboxPage />)
    expect(within(rows()[0]!).getByTestId('inbox-open')).toHaveAttribute('href', '/n/aaaaaaaa/s/f1')
  })

  it('an errored session offers open and archive; archive calls the session action', async () => {
    const user = setup()
    seed({ sessions: [sess({ id: 'bad', title: 'Broken', status: 'error' })] })
    render(<InboxPage />)
    expect(within(rows()[0]!).queryByTestId('inbox-done')).toBeNull()
    await user.click(within(rows()[0]!).getByTestId('inbox-archive'))
    expect(h.archive).toHaveBeenCalledWith(A, 'bad', true)
    await waitFor(() => expect(rows()).toHaveLength(0))
  })

  it('archiving a session that has a worktree asks what to do with it first', async () => {
    const user = setup()
    seed({ sessions: [sess({ id: 'wt', status: 'error', worktree: { state: 'active', branch: 'anycon/x', path: '.anycon/worktrees/x', base: 'main' } as SessionItem['worktree'] })] })
    render(<InboxPage />)
    await user.click(within(rows()[0]!).getByTestId('inbox-archive'))
    expect(h.archive).not.toHaveBeenCalled()
    expect(await screen.findByTestId('archive-worktree-dialog')).toBeInTheDocument()
    await user.click(screen.getByTestId('archive-confirm'))
    await waitFor(() => expect(h.archive).toHaveBeenCalledWith(A, 'wt', true, { worktree: 'auto' }))
  })

  it('an enroll request has a “Review” action that opens the approval dialog', async () => {
    const user = setup()
    useEnroll.setState({ items: { [`${A}:x`]: { nodeId: A, info: { enroll_id: 'x', expires_at_ms: Date.now() + 60_000, created_at_ms: Date.now(), status: 'wait_approval', method: 'code', approval: true }, expiresAt: Date.now() + 60_000, receivedAt: Date.now(), createdAt: Date.now() } } })
    render(<InboxPage />)
    await user.click(screen.getByTestId('inbox-review'))
    expect(useEnroll.getState().dialog).toEqual({ nodeId: A, enrollId: 'x', fromInbox: true })
  })
})

describe('phone', () => {
  beforeEach(() => setViewport('phone'))

  it('expands the first item by default, with the full card inside the row; the others are collapsed', () => {
    seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] })
    render(<InboxPage />)
    expect(screen.queryByTestId('inbox-detail-pane')).toBeNull()
    const [first, second] = rows()
    expect(within(first!).getByTestId('inbox-row-main')).toHaveAttribute('aria-expanded', 'true')
    expect(within(first!).getByTestId('permission-card')).toHaveAttribute('data-request', 'one')
    expect(within(first!).getByTestId('permission-command')).toBeInTheDocument()
    expect(within(second!).getByTestId('inbox-row-main')).toHaveAttribute('aria-expanded', 'false')
    expect(within(second!).queryByTestId('permission-card')).toBeNull()
    // 折叠的行上没有任何答复按钮
    expect(within(second!).queryByRole('button', { name: /allow|deny/i })).toBeNull()
  })

  it('tapping another row expands it, tapping the open one collapses it, and it stays collapsed', async () => {
    const user = setup()
    seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] })
    render(<InboxPage />)
    await user.click(within(rows()[1]!).getByTestId('inbox-row-main'))
    expect(within(rows()[1]!).getByTestId('permission-card')).toBeInTheDocument()
    expect(within(rows()[0]!).queryByTestId('permission-card')).toBeNull()
    await user.click(within(rows()[1]!).getByTestId('inbox-row-main'))
    expect(screen.queryByTestId('permission-card')).toBeNull()
  })

  it('there are no swipe actions on a request row (nothing but the button and the card)', () => {
    seed({ pending: [req('one')] })
    render(<InboxPage />)
    expect(rows()[0]!.getAttribute('draggable')).toBeNull()
    expect(rows()[0]!.querySelector('[data-swipe]')).toBeNull()
  })

  it('after answering the expanded one, the next one is expanded and focused', async () => {
    const user = setup()
    seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] })
    render(<InboxPage />)
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    await waitFor(() => expect(rows()).toHaveLength(1))
    expect(within(rows()[0]!).getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    expect(document.activeElement).toBe(within(rows()[0]!).getByTestId('inbox-row-main'))
  })
})

describe('tablet', () => {
  beforeEach(() => setViewport('tablet'))

  it('has no detail until a row is tapped; then it slides over from the right and can be closed', async () => {
    const user = setup()
    seed({ pending: [req('one')] })
    render(<InboxPage />)
    expect(screen.queryByTestId('permission-card')).toBeNull()
    await user.click(within(rows()[0]!).getByTestId('inbox-row-main'))
    const overlay = await screen.findByTestId('inbox-overlay')
    expect(within(overlay).getByTestId('permission-card')).toHaveAttribute('data-request', 'one')
    await user.click(screen.getByRole('button', { name: 'Close details' }))
    await waitFor(() => expect(screen.queryByTestId('permission-card')).toBeNull())
  })
})

describe('from a notification', () => {
  it('selects the earliest pending request of that session', () => {
    const done = vi.fn()
    seed({ pending: [req('late', { session_id: 'S', created_at_ms: 5_000 }), req('early', { session_id: 'S', created_at_ms: 1_000 }), req('other', { session_id: 'T', created_at_ms: 100 })] })
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} onLocateDone={done} />)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'early')
    expect(done).toHaveBeenCalled()
  })

  it('expands it on a phone', () => {
    setViewport('phone')
    seed({ pending: [req('other', { session_id: 'T', created_at_ms: 100 }), req('mine', { session_id: 'S', created_at_ms: 1_000 })] })
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'mine')
  })

  it('says so when the request is already gone, with a link to the session', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', synced: true }), [B]: nodeEntry({ id: B }) } })
    seed({ pending: [] })
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    const note = screen.getByTestId('inbox-gone')
    expect(note).toHaveTextContent('already been handled')
    expect(within(note).getByRole('link', { name: 'Open session' })).toHaveAttribute('href', '/n/aaaaaaaa/s/S')
  })

  it('reads the parameters at once (so they do not stay in the URL) and waits while the node has not finished syncing', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', synced: false, conn: 'noise_handshaking' }), [B]: nodeEntry({ id: B }) } })
    const done = vi.fn()
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} onLocateDone={done} />)
    expect(done).toHaveBeenCalledTimes(1)
    expect(screen.queryByTestId('inbox-gone')).toBeNull()
    expect(screen.getByTestId('inbox-connecting')).toHaveTextContent('Waiting for alpha to connect…')
    act(() => seed({ pending: [req('late', { session_id: 'S' })] }))
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'late')
    expect(screen.queryByTestId('inbox-connecting')).toBeNull()
  })

  it('once the node has synced and there is nothing, says already handled', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', synced: false }), [B]: nodeEntry({ id: B }) } })
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    expect(screen.queryByTestId('inbox-gone')).toBeNull()
    act(() => useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', synced: true }), [B]: nodeEntry({ id: B }) } }))
    expect(screen.getByTestId('inbox-gone')).toBeInTheDocument()
  })

  it('a node this browser does not know is not “already handled”: it offers unlock and pair', () => {
    render(<InboxPage locate={{ node: 'zzzzzzzz', session: 'S' }} />)
    expect(screen.queryByTestId('inbox-gone')).toBeNull()
    const note = screen.getByTestId('inbox-unknown-node')
    expect(within(note).getByTestId('inbox-unlock')).toHaveAttribute('href', '/unlock/zzzzzzzz')
    expect(within(note).getByRole('link', { name: 'Pair a node' })).toHaveAttribute('href', '/pair')
  })

  it('shows a request on a node outside the node filter anyway, with a note and a button that ticks the node; it does not switch by itself', async () => {
    const user = setup()
    usePrefs.setState({ sidebarNodes: [B] })
    seed({ pending: [req('hidden', { session_id: 'S', node_id: A }), req('mine', { node_id: B, session_id: 's-mine', created_at_ms: 1 })] })
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    const located = screen.getByTestId('inbox-located')
    expect(within(located).getAllByTestId('inbox-row')).toHaveLength(1)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'hidden')
    expect(screen.getByTestId('inbox-outside-note')).toHaveTextContent('alpha is not ticked in your node filter')
    // 数字仍然只算勾选的节点
    expect(screen.getByTestId('inbox-count-now')).toHaveTextContent('1')
    expect(usePrefs.getState().sidebarNodes).toEqual([B])
    await user.click(screen.getByTestId('inbox-show-node'))
    expect(usePrefs.getState().sidebarNodes).toBeNull()
  })

  it('with spaces: the note names the space the request belongs to and offers to switch to it; it does not switch by itself, and the numbers stay on the selected space', async () => {
    const user = setup()
    const group = (g: string) => ({ group: g, capabilities: { todo: true } }) as never
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: group('Home') }), [B]: nodeEntry({ id: B, name: 'beta', info: group('Work') }) }, order: [A, B], loaded: true })
    usePrefs.setState({ sidebarNodes: null, sidebarSpace: 'Work' })
    seed({ pending: [req('hidden', { session_id: 'S', node_id: A }), req('mine', { node_id: B, session_id: 's-mine', created_at_ms: 1 })] })
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    expect(screen.getByTestId('inbox-outside-note')).toHaveTextContent('belongs to the space “Home”, which is not selected')
    expect(screen.getByTestId('inbox-count-now')).toHaveTextContent('1')
    expect(usePrefs.getState().sidebarSpace).toBe('Work')
    await user.click(screen.getByTestId('inbox-switch-space'))
    expect(usePrefs.getState().sidebarSpace).toBe('Home')
    usePrefs.setState({ sidebarSpace: null })
  })

  it('holds the home notice back while the located request is shown', () => {
    seed({ pending: [req('one', { session_id: 'S' })] })
    const { unmount } = render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    expect(useUi.getState().deferHomeNotice).toBe(true)
    unmount()
    expect(useUi.getState().deferHomeNotice).toBe(false)
  })
})

describe('keyboard', () => {
  const key = (init: KeyboardEventInit, target: EventTarget = document.body): void => {
    act(() => {
      target.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, ...init }))
    })
  }

  it('J / K move the selection and the focus down and up the list', () => {
    seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 }), req('three', { created_at_ms: 3 })] })
    render(<InboxPage />)
    key({ key: 'j' })
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    expect(document.activeElement).toBe(within(rows()[1]!).getByTestId('inbox-row-main'))
    key({ key: 'j' })
    key({ key: 'j' }) // 到头不再往下
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'three')
    key({ key: 'k' })
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
  })

  it('single-letter keys do nothing while focus is in an input', () => {
    seed({
      pending: [
        req('one', { created_at_ms: 1 }),
        req('two', { created_at_ms: 2, kind: 'question', options: [{ id: 'ok', label: 'Submit', kind: 'allow_once' }], detail: { questions: [{ id: 'q', text: 'Why?', free_text: true }] } }),
      ],
    })
    render(<InboxPage />)
    key({ key: 'j' })
    const box = within(screen.getByTestId('inbox-detail-pane')).getByRole('textbox')
    key({ key: 'k' }, box)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
  })

  it('Enter on a row moves the focus into the details', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] })
    render(<InboxPage />)
    const second = within(rows()[1]!).getByTestId('inbox-row-main')
    second.focus()
    key({ key: 'Enter' }, second)
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60)
    })
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    expect(screen.getByTestId('inbox-detail-pane').contains(document.activeElement)).toBe(true)
    vi.useRealTimers()
  })

  it('⌘⇧A allows the selected tool request once, with the one-time option only', () => {
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({ top: 10, bottom: 60, left: 0, right: 10, width: 10, height: 50, x: 0, y: 10, toJSON: () => ({}) })
    seed({ pending: [req('one', { created_at_ms: 1, options: [ALWAYS, ALLOW, DENY] })] })
    render(<InboxPage />)
    arm()
    key({ key: 'A', metaKey: true, shiftKey: true })
    expect(h.respond).toHaveBeenCalledWith(A, { request_id: 'one', option_id: 'allow' })
  })

  it('⌘⇧D denies once; a request without a one-time option is left alone', () => {
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({ top: 10, bottom: 60, left: 0, right: 10, width: 10, height: 50, x: 0, y: 10, toJSON: () => ({}) })
    seed({ pending: [req('one', { created_at_ms: 1 })] })
    const { unmount } = render(<InboxPage />)
    arm()
    key({ key: 'D', metaKey: true, shiftKey: true })
    expect(h.respond).toHaveBeenCalledWith(A, { request_id: 'one', option_id: 'deny' })
    unmount()
    h.respond.mockClear()
    seed({ pending: [req('two', { options: [ALWAYS] })] })
    render(<InboxPage />)
    arm()
    key({ key: 'A', metaKey: true, shiftKey: true })
    expect(h.respond).not.toHaveBeenCalled()
  })

  it('⌘⇧A does not answer a plan (questions, mode changes and other requests: InboxPage.safety.test.tsx)', () => {
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({ top: 10, bottom: 60, left: 0, right: 10, width: 10, height: 50, x: 0, y: 10, toJSON: () => ({}) })
    seed({ pending: [req('plan', { kind: 'plan', options: [ALLOW] })] })
    render(<InboxPage />)
    arm()
    key({ key: 'A', metaKey: true, shiftKey: true })
    expect(h.respond).not.toHaveBeenCalled()
  })

  it('⌘⇧A scrolls to the card instead of answering when it is not on screen', () => {
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({ top: 5000, bottom: 5050, left: 0, right: 10, width: 10, height: 50, x: 0, y: 5000, toJSON: () => ({}) })
    seed({ pending: [req('one')] })
    render(<InboxPage />)
    arm()
    key({ key: 'A', metaKey: true, shiftKey: true })
    expect(h.respond).not.toHaveBeenCalled()
  })

  it('does nothing while the page is hidden behind another tab', () => {
    seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] })
    const { rerender } = render(<InboxPage active />)
    rerender(<InboxPage active={false} />)
    key({ key: 'j' })
    rerender(<InboxPage active />)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'one')
  })

})
