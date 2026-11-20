// 「在这个页面上处理的」只算「待处理」页在眼前时自己发起的；藏在后面时（手机上切到会话页）别处的答复是「别处处理的」。
// 另外：重新显示时重新计时、页面打开时是空的就不替用户展开后来的条目、过滤器改了不留占位、非请求条目的占位、
// 接入请求在对话框里点了才算、平板抽屉的焦点、深链接晚到不抢选中、相对时间每分钟走。
import { act, render, renderHook, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { PermissionKind, PermissionOption } from '@/protocol'
import { ARM_DELAY_MS } from '@/features/inbox/arming'
import { beginAnswer, holdHandled, useInflight } from '@/features/session/inflight'
import { nodeEntry, summary } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { resetViewport, setViewport } from '@/test/viewport'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type PendingPermission, type SessionItem } from '@/stores/sessions'

const h = vi.hoisted(() => ({ navigate: vi.fn(), respond: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, {}, { pathname: '/inbox' }))
vi.mock('@/features/session/actions', async (orig) => ({ ...(await orig<object>()), respondPermission: h.respond }))

import { useNodeFilter } from '@/lib/nodeFilter'
import { InboxPage } from './InboxPage'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const ALLOW: PermissionOption = { id: 'allow', label: 'Allow once', kind: 'allow_once' }
const DENY: PermissionOption = { id: 'deny', label: 'Deny once', kind: 'reject_once' }

const sess = (over: Partial<SessionItem> & { id: string }): SessionItem => ({ ...summary({ title: over.id, project: 'app', updated_at_ms: Date.now() - 60_000, ...over }), node_id: A, cached_seq: 0, ...over })
const req = (id: string, over: Partial<PendingPermission> = {}): PendingPermission => ({
  request_id: id,
  session_id: `s-${id}`,
  node_id: A,
  kind: 'tool' as PermissionKind,
  title: `Run ${id}`,
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: `echo ${id}` }, cwd: `/work/${id}`, detail: { command: `echo ${id}` } } },
  options: [ALLOW, DENY],
  created_at_ms: Date.now() - 120_000,
  expires_at_ms: null,
  ...over,
})
const seed = (o: { sessions?: SessionItem[]; pending?: PendingPermission[] }): void =>
  useSessions.setState({ list: Object.fromEntries((o.sessions ?? []).map((s) => [s.id, s])), pending: Object.fromEntries((o.pending ?? []).map((p) => [p.request_id, p])), open: {} })
const twoRequests = (): void => seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] })
const arm = (): void => {
  act(() => void vi.advanceTimersByTime(ARM_DELAY_MS + 10))
}
const setup = () => userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
const rows = (): HTMLElement[] => screen.queryAllByTestId('inbox-row')
const resolveElsewhere = (id: string): void => act(() => useSessions.getState().resolvePending(A, id))

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true })
  useInflight.setState({ ids: new Set(), handled: new Set() })
  h.navigate.mockReset()
  h.respond.mockReset().mockImplementation(async (nodeId: string, p: { request_id: string }) => {
    useSessions.getState().resolvePending(nodeId, p.request_id)
    return 'answered'
  })
  resetViewport()
  usePrefs.setState({ sidebarNodes: null })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha' }), [B]: nodeEntry({ id: B, name: 'beta' }) }, order: [A, B], loaded: true })
  seed({})
})
afterEach(() => vi.useRealTimers())

describe('a hidden Inbox tab (phone) while the same request is answered on the session page', () => {
  it('is “handled elsewhere” for the Inbox: coming back shows the placeholder, nothing else is expanded or clickable', () => {
    setViewport('phone')
    twoRequests()
    const { rerender } = render(<InboxPage active />)
    expect(within(rows()[0]!).getByTestId('permission-card')).toHaveAttribute('data-request', 'one')
    // 切到会话页：Inbox 标签页藏起来；在会话页答复 r1（会话页的按钮 / 快捷键不带「在这里处理」）
    rerender(<InboxPage active={false} />)
    act(() => void beginAnswer('one'))
    resolveElsewhere('one')
    act(() => void useInflight.setState((s) => ({ ids: new Set([...s.ids].filter((i) => i !== 'one')) })))
    rerender(<InboxPage active />)
    expect(screen.getByTestId('inbox-ghost')).toBeInTheDocument()
    expect(screen.queryByTestId('permission-card')).toBeNull()
    expect(rows()).toHaveLength(1)
    expect(within(rows()[0]!).getByTestId('inbox-row-main')).toHaveAttribute('aria-expanded', 'false')
  })

  it('an answer from the Inbox while it is on screen still advances', async () => {
    setViewport('phone')
    const user = setup()
    twoRequests()
    render(<InboxPage active />)
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    await waitFor(() => expect(within(rows()[0]!).getByTestId('permission-card')).toHaveAttribute('data-request', 'two'))
    expect(screen.queryByTestId('inbox-ghost')).toBeNull()
  })

  it('an answer given by the Inbox’s own hidden copy is not “handled here”', () => {
    setViewport('phone')
    twoRequests()
    const { rerender } = render(<InboxPage active />)
    rerender(<InboxPage active={false} />)
    // 藏着的页面里的卡片按钮点不到；就算有答复经过它的 hook，也不算在这里处理
    expect(useInflight.getState().handled.size).toBe(0)
  })
})

describe('arming restarts when the detail becomes visible again', () => {
  it('a card that was armed before the tab was hidden is locked again when the tab comes back, buttons and shortcut', () => {
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({ top: 10, bottom: 60, left: 0, right: 10, width: 10, height: 50, x: 0, y: 10, toJSON: () => ({}) })
    setViewport('phone')
    seed({ pending: [req('one')] })
    const { rerender } = render(<InboxPage active />)
    arm()
    expect(screen.getByTestId('permission-option-allow')).toBeEnabled()
    rerender(<InboxPage active={false} />)
    act(() => void vi.advanceTimersByTime(ARM_DELAY_MS * 3))
    rerender(<InboxPage active />)
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    act(() => void document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'A', metaKey: true, shiftKey: true, bubbles: true })))
    expect(h.respond).not.toHaveBeenCalled()
    arm()
    expect(screen.getByTestId('permission-option-allow')).toBeEnabled()
  })
})

describe('phone default expansion', () => {
  it('is decided once, when the page first appears: a page opened empty never expands a later arrival', () => {
    setViewport('phone')
    render(<InboxPage active />)
    act(() => seed({ pending: [req('late')] }))
    expect(screen.queryByTestId('permission-card')).toBeNull()
    expect(within(rows()[0]!).getByTestId('inbox-row-main')).toHaveAttribute('aria-expanded', 'false')
  })

  it('but a cold start still expands the first item: while the nodes are still syncing the page has not “opened” yet', () => {
    setViewport('phone')
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', conn: 'noise_handshaking', synced: false }), [B]: nodeEntry({ id: B, name: 'beta', conn: 'noise_handshaking', synced: false }) } })
    render(<InboxPage active />)
    act(() => seed({ pending: [req('arrives', { created_at_ms: 1 })] }))
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'arrives')
    // 同步完之后才到的不再展开
    act(() => seed({ pending: [req('arrives', { created_at_ms: 1 }), req('later', { created_at_ms: 2 })] }))
    expect(screen.getAllByTestId('permission-card')).toHaveLength(1)
  })

  it('and not while the tab is hidden', () => {
    setViewport('phone')
    seed({ pending: [req('one')] })
    const { rerender } = render(<InboxPage active />)
    rerender(<InboxPage active={false} />)
    resolveElsewhere('one')
    act(() => seed({ pending: [req('two')] }))
    rerender(<InboxPage active />)
    expect(screen.queryByTestId('permission-card')).toBeNull()
  })
})

describe('when an item leaves', () => {
  it('because the user unticked its node there is no placeholder: the selection is just cleared', () => {
    seed({ pending: [req('one', { created_at_ms: 1 }), req('b1', { node_id: B, session_id: 's-b1', created_at_ms: 2 })] })
    render(<InboxPage />)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'one')
    const filter = renderHook(() => useNodeFilter())
    act(() => filter.result.current.toggle(A, false)) // 用户在切换器里取消勾选
    expect(screen.queryByTestId('inbox-ghost')).toBeNull()
    expect(rows()).toHaveLength(1)
  })

  it('a follow-up mark cleared elsewhere leaves the neutral placeholder, with Open session', () => {
    seed({ sessions: [sess({ id: 'f', title: 'Flagged', follow_up: true, status: 'idle' })] })
    render(<InboxPage />)
    expect(rows()[0]).toHaveAttribute('data-selected', 'true')
    act(() => seed({ sessions: [sess({ id: 'f', title: 'Flagged', follow_up: false, status: 'idle' })] }))
    const ghost = screen.getByTestId('inbox-ghost')
    expect(ghost).toHaveTextContent('No longer waiting for you')
    expect(ghost).not.toHaveTextContent('request was answered')
    expect(within(ghost).getByTestId('inbox-ghost-open')).toHaveAttribute('href', '/n/aaaaaaaa/s/f')
  })

  it('so does an error that was archived elsewhere, and an enroll request that expired', () => {
    seed({ sessions: [sess({ id: 'e', status: 'error', updated_at_ms: Date.now() })] })
    render(<InboxPage />)
    act(() => seed({ sessions: [sess({ id: 'e', status: 'error', archived: true, updated_at_ms: Date.now() })] }))
    expect(screen.getByTestId('inbox-ghost')).toBeInTheDocument()
  })
})

describe('device requests', () => {
  const enroll = () => ({ nodeId: A, info: { enroll_id: 'x', expires_at_ms: Date.now() + 300_000, created_at_ms: Date.now(), status: 'wait_approval' as const, method: 'code' as const, approval: true }, expiresAt: Date.now() + 300_000, receivedAt: Date.now(), createdAt: Date.now() })
  const gone = (): void => act(() => useEnroll.setState({ items: {} }))

  it('opening the dialog and cancelling is not “handled here”: when another device approves, the placeholder shows', async () => {
    const user = setup()
    useEnroll.setState({ items: { [`${A}:x`]: enroll() } })
    render(<InboxPage />)
    await user.click(screen.getByTestId('inbox-review'))
    vi.advanceTimersByTime(2000)
    gone()
    expect(screen.getByTestId('inbox-ghost')).toBeInTheDocument()
  })

  it('approving or denying in the dialog (which marks it) advances, even long after the dialog opened', () => {
    useEnroll.setState({ items: { [`${A}:x`]: enroll(), [`${A}:y`]: { ...enroll(), info: { ...enroll().info, enroll_id: 'y' }, createdAt: Date.now() + 1 } } })
    render(<InboxPage />)
    act(() => holdHandled(`enroll:${A}:x`, 300_000)) // 对话框里点了批准 / 拒绝（从这里打开的）会这样标
    act(() => void vi.advanceTimersByTime(60_000))
    act(() => useEnroll.setState((s) => ({ items: { [`${A}:y`]: s.items[`${A}:y`]! } })))
    expect(screen.queryByTestId('inbox-ghost')).toBeNull()
    expect(rows()[0]).toHaveAttribute('data-selected', 'true')
  })
})

describe('tablet drawer focus', () => {
  it('after the last item is handled in the drawer, focus goes to the empty heading at once (the page places it, Radix does not take it back)', async () => {
    setViewport('tablet')
    const user = setup()
    seed({ pending: [req('only')] })
    render(<InboxPage />)
    await user.click(within(rows()[0]!).getByTestId('inbox-row-main'))
    await screen.findByTestId('inbox-overlay')
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    await waitFor(() => expect(screen.queryByTestId('inbox-overlay')).toBeNull())
    expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Nothing is waiting for you' }))
    act(() => void vi.advanceTimersByTime(300))
    expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Nothing is waiting for you' }))
  })
})

describe('notification target', () => {
  it('does not take the selection away from what the user picked after the link was opened', async () => {
    const user = setup()
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', synced: false, conn: 'noise_handshaking' }), [B]: nodeEntry({ id: B }) } })
    seed({ pending: [req('other', { session_id: 'T', created_at_ms: 1 })] })
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    await user.click(within(rows()[0]!).getByTestId('inbox-row-main'))
    act(() => seed({ pending: [req('other', { session_id: 'T', created_at_ms: 1 }), req('target', { session_id: 'S', created_at_ms: 2 })] }))
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'other')
  })

  it('says an offline node is offline, and one that no longer recognises this browser needs unlocking', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', synced: false, conn: 'peer_offline' }), [B]: nodeEntry({ id: B }) } })
    const { unmount } = render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    expect(screen.getByTestId('inbox-connecting')).toHaveTextContent('alpha is offline')
    unmount()
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', synced: false, conn: 'noise_failed' }), [B]: nodeEntry({ id: B }) } })
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    expect(screen.getByTestId('inbox-connecting')).toHaveTextContent('no longer recognises this browser')
  })
})

describe('relative times', () => {
  it('tick every minute even though the model does not change', () => {
    seed({ pending: [req('one', { created_at_ms: Date.now() })] })
    render(<InboxPage />)
    const before = within(rows()[0]!).getByTestId('inbox-row-main').textContent
    act(() => void vi.advanceTimersByTime(5 * 60_000))
    expect(within(rows()[0]!).getByTestId('inbox-row-main').textContent).not.toBe(before)
  })
})
