// 「待处理」页的加固（spec/web.md § 4.11）：从通知点进来的、在过滤器之外的条目和别的条目走同样的占位 / 前进规则；
// 页面藏在后面时什么都不选、不前进、不展开；冷启动的默认展开不在藏着的时候发生；答复的「在这里处理」只认眼前的「待处理」；
// 平板抽屉关掉时焦点回到存在的东西上。
import { act, render, renderHook, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { PermissionKind, PermissionOption } from '@/protocol'
import { InboxActiveContext } from '@/features/inbox/active'
import { ARM_DELAY_MS } from '@/features/inbox/arming'
import { deriveInbox, nowItems, type RequestItem } from '@/features/inbox/derive'
import { beginAnswer, useInflight } from '@/features/session/inflight'
import { nodeEntry } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { resetViewport, setViewport } from '@/test/viewport'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type PendingPermission } from '@/stores/sessions'

const h = vi.hoisted(() => ({ navigate: vi.fn(), respond: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, {}, { pathname: '/inbox' }))
vi.mock('@/features/session/actions', async (orig) => ({ ...(await orig<object>()), respondPermission: h.respond }))

import { useNodeFilter } from '@/lib/nodeFilter'
import { InboxPage } from './InboxPage'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const ALLOW: PermissionOption = { id: 'allow', label: 'Allow once', kind: 'allow_once' }
const DENY: PermissionOption = { id: 'deny', label: 'Deny once', kind: 'reject_once' }

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
const seedPending = (...p: PendingPermission[]): void => useSessions.setState({ list: {}, pending: Object.fromEntries(p.map((x) => [x.request_id, x])), open: {} })
const arm = (): void => {
  act(() => void vi.advanceTimersByTime(ARM_DELAY_MS + 10))
}
const setup = () => userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
const rows = (): HTMLElement[] => screen.queryAllByTestId('inbox-row')
const resolveElsewhere = (id: string): void => act(() => useSessions.getState().resolvePending(A, id))
const syncing = (): void => useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', conn: 'noise_handshaking', synced: false }), [B]: nodeEntry({ id: B, name: 'beta' }) } })

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
  seedPending()
})
afterEach(() => vi.useRealTimers())

describe('the located item above the list (its node is not ticked)', () => {
  const locate = { node: 'aaaaaaaa', session: 's-hidden' }
  const setupLocated = () => {
    usePrefs.setState({ sidebarNodes: [B] })
    seedPending(req('hidden', { session_id: 's-hidden', node_id: A }), req('mine', { node_id: B, session_id: 's-mine', created_at_ms: 1 }))
    render(<InboxPage locate={locate} />)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'hidden')
  }

  it('answered somewhere else: the placeholder, not a silent clear', () => {
    setupLocated()
    resolveElsewhere('hidden')
    expect(screen.getByTestId('inbox-ghost')).toBeInTheDocument()
    expect(screen.queryByTestId('permission-card')).toBeNull()
  })

  it('answered here: advances to the next item and the “handled here” mark is consumed', async () => {
    const user = setup()
    setupLocated()
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    await waitFor(() => expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'mine'))
    expect(useInflight.getState().handled.has('request:hidden')).toBe(false)
    expect(screen.queryByTestId('inbox-ghost')).toBeNull()
  })

  it('a real untick of the selected item’s node still just clears the selection', () => {
    seedPending(req('one', { created_at_ms: 1 }), req('b1', { node_id: B, session_id: 's-b1', created_at_ms: 2 }))
    render(<InboxPage />)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'one')
    const filter = renderHook(() => useNodeFilter())
    act(() => filter.result.current.toggle(A, false)) // 用户在切换器里取消勾选
    expect(screen.queryByTestId('inbox-ghost')).toBeNull()
  })
})

describe('while the page is hidden (phone) nothing is advanced, selected or expanded', () => {
  beforeEach(() => setViewport('phone'))

  it('a mark set while visible but consumed while hidden does not advance: the placeholder shows and the mark stays unconsumed', () => {
    seedPending(req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 }))
    const { rerender } = render(<InboxPage active />)
    expect(within(rows()[0]!).getByTestId('permission-card')).toHaveAttribute('data-request', 'one')
    act(() => void beginAnswer('one', true))
    rerender(<InboxPage active={false} />)
    resolveElsewhere('one')
    rerender(<InboxPage active />)
    expect(screen.getByTestId('inbox-ghost')).toBeInTheDocument()
    expect(screen.queryByTestId('permission-card')).toBeNull()
    expect(rows()).toHaveLength(1)
    expect(within(rows()[0]!).getByTestId('inbox-row-main')).toHaveAttribute('aria-expanded', 'false')
  })

  it('a filter change while hidden clears the selection and nothing else is expanded when the page comes back', () => {
    seedPending(req('one', { created_at_ms: 1 }), req('b1', { node_id: B, session_id: 's-b1', created_at_ms: 2 }))
    const { rerender } = render(<InboxPage active />)
    rerender(<InboxPage active={false} />)
    const filter = renderHook(() => useNodeFilter())
    act(() => filter.result.current.toggle(A, false)) // 用户在切换器里取消勾选
    rerender(<InboxPage active />)
    expect(screen.queryByTestId('permission-card')).toBeNull()
    expect(screen.queryByTestId('inbox-ghost')).toBeNull()
  })

  it('cold start: an item arriving while the page is hidden stays collapsed; when the page is shown it is expanded and locked for the arming delay', () => {
    syncing()
    const { rerender } = render(<InboxPage active />)
    rerender(<InboxPage active={false} />)
    act(() => seedPending(req('arrives')))
    // 节点在藏着的时候同步完了（页面没在眼前，照样什么都不展开）
    act(() => useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha' }), [B]: nodeEntry({ id: B, name: 'beta' }) } }))
    expect(screen.queryByTestId('permission-card')).toBeNull()
    expect(within(rows()[0]!).getByTestId('inbox-row-main')).toHaveAttribute('aria-expanded', 'false')
    rerender(<InboxPage active />)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'arrives')
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    arm()
    expect(screen.getByTestId('permission-option-allow')).toBeEnabled()
  })
})

describe('what counts as answering “here”', () => {
  const item = (): RequestItem => {
    seedPending(req('one'))
    return nowItems(deriveInbox({ sessions: {}, pending: useSessions.getState().pending, enroll: { items: {}, resolved: {} }, selected: [A], now: Date.now() }))[0] as RequestItem
  }
  const answer = async (value: boolean | undefined) => {
    const user = setup()
    const { InboxDetail } = await import('@/features/inbox/ui/InboxItems')
    const it = item()
    const ui = <InboxDetail item={it} armed />
    render(value === undefined ? ui : <InboxActiveContext.Provider value={value}>{ui}</InboxActiveContext.Provider>)
    await user.click(screen.getByTestId('permission-option-allow'))
    await waitFor(() => expect(h.respond).toHaveBeenCalled())
  }

  it('a card in a hidden Inbox (context false) that answers sets no mark', async () => {
    await answer(false)
    expect(useInflight.getState().handled.has('request:one')).toBe(false)
  })

  it('a card in the visible Inbox (context true) does', async () => {
    await answer(true)
    expect(useInflight.getState().handled.has('request:one')).toBe(true)
  })

  it('a card outside any provider never marks', async () => {
    await answer(undefined)
    expect(useInflight.getState().handled.has('request:one')).toBe(false)
  })
})

describe('tablet drawer focus', () => {
  it('after advancing to the next item inside the drawer, closing it returns focus to the selected row, not to the removed one', async () => {
    setViewport('tablet')
    const user = setup()
    seedPending(req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 }))
    render(<InboxPage />)
    await user.click(within(rows()[0]!).getByTestId('inbox-row-main'))
    await screen.findByTestId('inbox-overlay')
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    await waitFor(() => expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two'))
    await user.keyboard('{Escape}')
    await waitFor(() => expect(screen.queryByTestId('inbox-overlay')).toBeNull())
    act(() => void vi.advanceTimersByTime(50))
    expect(document.activeElement).toBe(within(rows()[0]!).getByTestId('inbox-row-main'))
    expect(rows()[0]).toHaveTextContent('Run two')
  })
})

describe('notification target on a node with an incompatible protocol', () => {
  it('says so instead of waiting for ever', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', synced: false, conn: 'ready', versionMismatch: true }), [B]: nodeEntry({ id: B }) } })
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 'S' }} />)
    expect(screen.getByTestId('inbox-connecting')).toHaveTextContent('incompatible protocol version')
  })
})
