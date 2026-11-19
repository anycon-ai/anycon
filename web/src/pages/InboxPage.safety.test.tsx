// 答复只能落在用户看过的请求上（spec/web.md § 4.11）：别处处理掉的不替用户挑下一条、新显示的请求先锁一小会儿、
// 同一个请求只答复一次、选中跟着条目而不是位置；以及没有请求在内存里的「等待审批」会话。
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { PermissionKind, PermissionOption } from '@/protocol'
import { ARM_DELAY_MS } from '@/features/inbox/arming'
import { useInflight } from '@/features/session/inflight'
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
const seed = (o: { sessions?: SessionItem[]; pending?: PendingPermission[] }): void =>
  useSessions.setState({ list: Object.fromEntries((o.sessions ?? []).map((s) => [s.id, s])), pending: Object.fromEntries((o.pending ?? []).map((p) => [p.request_id, p])), open: {} })

const arm = (): void => {
  act(() => void vi.advanceTimersByTime(ARM_DELAY_MS + 10))
}
const setup = () => userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
const answerKey = (init: KeyboardEventInit = { key: 'A' }): void => {
  act(() => void document.body.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, metaKey: true, shiftKey: true, ...init })))
}
const onScreen = (): void => {
  vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({ top: 10, bottom: 60, left: 0, right: 10, width: 10, height: 50, x: 0, y: 10, toJSON: () => ({}) })
}
const resolveElsewhere = (id: string): void => act(() => useSessions.getState().resolvePending(A, id))
const twoRequests = (): void => seed({ pending: [req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] })
const rows = (): HTMLElement[] => screen.queryAllByTestId('inbox-row')

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

describe('a request that goes away without being handled here', () => {
  it('desktop: the pane shows “handled elsewhere” with Next and Open session, and nothing else is selected or answerable', async () => {
    const user = setup()
    onScreen()
    twoRequests()
    render(<InboxPage />)
    arm()
    resolveElsewhere('one')
    const ghost = screen.getByTestId('inbox-ghost')
    expect(ghost).toHaveTextContent('Handled elsewhere')
    expect(within(ghost).getByTestId('inbox-ghost-open')).toHaveAttribute('href', '/n/aaaaaaaa/s/s-one')
    expect(screen.queryByTestId('permission-card')).toBeNull()
    expect(rows()[0]).not.toHaveAttribute('data-selected')
    // ⌘⇧A 不会落到补位上来的那一条
    arm()
    answerKey()
    expect(h.respond).not.toHaveBeenCalled()
    // 用户自己点「下一条」，才选中它；它的按钮先锁一小会儿
    await user.click(screen.getByTestId('inbox-ghost-next'))
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    arm()
    expect(screen.getByTestId('permission-option-allow')).toBeEnabled()
  })

  it('a request answered here still advances to the next one, unlike one handled elsewhere', async () => {
    const user = setup()
    twoRequests()
    render(<InboxPage />)
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    await waitFor(() => expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two'))
    expect(screen.queryByTestId('inbox-ghost')).toBeNull()
  })

  it('a failed answer is not “handled here”', async () => {
    const user = setup()
    h.respond.mockRejectedValueOnce(new Error('boom'))
    twoRequests()
    render(<InboxPage />)
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    await waitFor(() => expect(useInflight.getState().ids.size).toBe(0))
    expect(useInflight.getState().handled.has('request:one')).toBe(false)
  })

  it('phone: the placeholder replaces the expanded row’s detail, and the next row is not expanded for the user', () => {
    setViewport('phone')
    twoRequests()
    render(<InboxPage />)
    expect(within(rows()[0]!).getByTestId('permission-card')).toBeInTheDocument()
    resolveElsewhere('one')
    expect(screen.getByTestId('inbox-ghost')).toBeInTheDocument()
    expect(screen.queryByTestId('permission-card')).toBeNull()
    expect(rows()).toHaveLength(1)
    expect(within(rows()[0]!).getByTestId('inbox-row-main')).toHaveAttribute('aria-expanded', 'false')
  })

  it('phone: it is not re-expanded just because the list changed, only when the page opens', () => {
    setViewport('phone')
    seed({ pending: [req('one', { created_at_ms: 1 })] })
    render(<InboxPage />)
    expect(screen.getByTestId('permission-card')).toBeInTheDocument()
    resolveElsewhere('one')
    act(() => seed({ pending: [req('later', { created_at_ms: 5 })] }))
    expect(screen.queryByTestId('permission-card')).toBeNull()
  })

  it('tablet: the drawer shows the placeholder instead of silently moving on', async () => {
    setViewport('tablet')
    const user = setup()
    twoRequests()
    render(<InboxPage />)
    await user.click(within(rows()[0]!).getByTestId('inbox-row-main'))
    await screen.findByTestId('inbox-overlay')
    resolveElsewhere('one')
    expect(within(screen.getByTestId('inbox-overlay')).getByTestId('inbox-ghost')).toBeInTheDocument()
    expect(screen.queryByTestId('permission-card')).toBeNull()
  })

  it('tablet: resizing down from desktop does not pop the drawer open for the auto-selected item', () => {
    twoRequests()
    const { rerender } = render(<InboxPage />)
    expect(screen.getByTestId('permission-card')).toBeInTheDocument()
    setViewport('tablet')
    rerender(<InboxPage />)
    expect(screen.getByTestId('inbox-page')).toHaveAttribute('data-mode', 'overlay')
    expect(screen.queryByTestId('inbox-overlay')).toBeNull()
  })
})

describe('the arming delay', () => {
  it('locks the buttons visibly for ARM_DELAY_MS after a request is shown, and a click during it does nothing', () => {
    seed({ pending: [req('one')] })
    render(<InboxPage />)
    const allow = screen.getByTestId('permission-option-allow')
    expect(allow).toBeDisabled()
    fireEvent.click(allow)
    act(() => void vi.advanceTimersByTime(ARM_DELAY_MS - 50))
    expect(allow).toBeDisabled()
    fireEvent.click(allow)
    expect(h.respond).not.toHaveBeenCalled()
    act(() => void vi.advanceTimersByTime(60))
    expect(allow).toBeEnabled()
  })

  it('⌘⇧A / ⌘⇧D during the delay do nothing, and work after it', () => {
    onScreen()
    seed({ pending: [req('one')] })
    render(<InboxPage />)
    answerKey()
    answerKey({ key: 'D' })
    expect(h.respond).not.toHaveBeenCalled()
    arm()
    answerKey()
    expect(h.respond).toHaveBeenCalledTimes(1)
  })

  it('every change of the shown request re-arms: moving with J right before ⌘⇧A does not answer the new one', () => {
    onScreen()
    twoRequests()
    render(<InboxPage />)
    arm()
    act(() => void document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'j', bubbles: true })))
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    answerKey()
    expect(h.respond).not.toHaveBeenCalled()
    arm()
    answerKey()
    expect(h.respond).toHaveBeenCalledWith(A, { request_id: 'two', option_id: 'allow' })
  })

  it('also after a deep link and after the user picks another row', async () => {
    const user = setup()
    twoRequests()
    render(<InboxPage locate={{ node: 'aaaaaaaa', session: 's-two' }} />)
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    arm()
    await user.click(within(rows()[0]!).getByTestId('inbox-row-main'))
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
  })

  it('on a phone and in the tablet drawer too', async () => {
    setViewport('phone')
    seed({ pending: [req('one')] })
    const { unmount } = render(<InboxPage />)
    expect(screen.getByTestId('permission-option-allow')).toBeDisabled()
    unmount()
    setViewport('tablet')
    const user = setup()
    render(<InboxPage />)
    await user.click(within(rows()[0]!).getByTestId('inbox-row-main'))
    expect(await screen.findByTestId('permission-option-allow')).toBeDisabled()
  })
})

describe('one answer per request', () => {
  it('a double click on “Allow once” sends one answer, and the next request that moves up is locked', async () => {
    const user = setup()
    let release: () => void = () => {}
    h.respond.mockImplementationOnce(
      (nodeId: string, p: { request_id: string }) =>
        new Promise((res) => {
          release = () => {
            useSessions.getState().resolvePending(nodeId, p.request_id)
            res('answered')
          }
        }),
    )
    twoRequests()
    render(<InboxPage />)
    arm()
    const allow = screen.getByTestId('permission-option-allow')
    await user.dblClick(allow)
    expect(h.respond).toHaveBeenCalledTimes(1)
    expect(allow).toBeDisabled()
    await act(async () => release())
    await waitFor(() => expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two'))
    // 补位上来的那条先锁着：第二下点击落不到它头上
    await user.click(screen.getByTestId('permission-option-allow'))
    expect(h.respond).toHaveBeenCalledTimes(1)
  })

  it('two quick ⌘⇧A presses send one answer for that request', () => {
    onScreen()
    h.respond.mockImplementationOnce(() => new Promise(() => {}))
    twoRequests()
    render(<InboxPage />)
    arm()
    answerKey()
    answerKey()
    expect(h.respond).toHaveBeenCalledTimes(1)
  })

  it('a button click and a shortcut for the same request share the in-flight record', async () => {
    const user = setup()
    onScreen()
    h.respond.mockImplementationOnce(() => new Promise(() => {}))
    seed({ pending: [req('one')] })
    render(<InboxPage />)
    arm()
    await user.click(screen.getByTestId('permission-option-allow'))
    answerKey()
    expect(h.respond).toHaveBeenCalledTimes(1)
  })
})

describe('selection follows the item, not the position', () => {
  it('a new request that sorts ahead of the one being read does not change what is shown', async () => {
    const user = setup()
    twoRequests()
    render(<InboxPage />)
    await user.click(within(rows()[1]!).getByTestId('inbox-row-main'))
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    act(() => seed({ pending: [req('urgent', { created_at_ms: 0, expires_at_ms: Date.now() + 1000 }), req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] }))
    expect(rows()).toHaveLength(3)
    expect(rows()[0]).toHaveTextContent('Run urgent')
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
    expect(screen.getAllByTestId('permission-card')).toHaveLength(1)
  })

  it('phone: the expanded row stays the same one when another is inserted above it', async () => {
    setViewport('phone')
    const user = setup()
    twoRequests()
    render(<InboxPage />)
    await user.click(within(rows()[1]!).getByTestId('inbox-row-main'))
    act(() => seed({ pending: [req('first', { created_at_ms: 0 }), req('one', { created_at_ms: 1 }), req('two', { created_at_ms: 2 })] }))
    expect(screen.getByTestId('permission-card')).toHaveAttribute('data-request', 'two')
  })
})

describe('⌘⇧A / ⌘⇧D restrictions', () => {
  const kinds: Array<[PermissionKind, Partial<PendingPermission>]> = [
    ['mode_change', { detail: { mode: 'yolo' }, options: [ALLOW, DENY] }],
    ['question', { detail: { questions: [{ id: 'q', text: 'Why?', choices: [{ id: 'a', label: 'A' }] }] }, options: [ALLOW] }],
    ['other', { detail: { text: 'Please confirm' }, options: [ALLOW, DENY] }],
    ['plan', { detail: { text: '# plan' }, options: [ALLOW, DENY] }],
  ]
  for (const [kind, over] of kinds) {
    it(`does nothing on a ${kind} request`, () => {
      onScreen()
      seed({ pending: [req('x', { kind, ...over })] })
      render(<InboxPage />)
      arm()
      answerKey()
      answerKey({ key: 'D' })
      expect(h.respond).not.toHaveBeenCalled()
    })
  }

  it('does nothing while a dialog or menu is open', () => {
    onScreen()
    seed({ pending: [req('one')] })
    render(<InboxPage />)
    arm()
    const dialog = document.createElement('div')
    dialog.setAttribute('role', 'dialog')
    document.body.append(dialog)
    answerKey()
    dialog.remove()
    expect(h.respond).not.toHaveBeenCalled()
    answerKey()
    expect(h.respond).toHaveBeenCalledTimes(1)
  })

  it('does nothing while the node is offline (the buttons are disabled too)', () => {
    onScreen()
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', conn: 'peer_offline' }), [B]: nodeEntry({ id: B }) } })
    seed({ pending: [req('one')] })
    render(<InboxPage />)
    arm()
    answerKey()
    expect(h.respond).not.toHaveBeenCalled()
  })
})

describe('sessions waiting for approval without a request', () => {
  const waiting = () => sess({ id: 'w', title: 'Needs OK', status: 'waiting_permission', pending_permissions: 1 })

  it('are rows labelled waiting (cached) that link to the session and have no answer button, and are replaced when the request arrives', () => {
    seed({ sessions: [waiting()] })
    render(<InboxPage />)
    const row = rows()[0]!
    expect(row).toHaveAttribute('data-kind', 'waiting')
    expect(row).toHaveTextContent('Waiting for approval (cached)')
    expect(within(row).getByTestId('inbox-cached')).toBeInTheDocument()
    expect(within(row).getByTestId('inbox-open')).toHaveAttribute('href', '/n/aaaaaaaa/s/w')
    expect(screen.queryByTestId('permission-card')).toBeNull()
    expect(screen.queryByTestId('inbox-in-progress')).toBeNull()
    act(() => seed({ sessions: [waiting()], pending: [req('r', { session_id: 'w' })] }))
    expect(rows()).toHaveLength(1)
    expect(rows()[0]).toHaveAttribute('data-kind', 'permission')
  })

  it('when it is the selected row and the real request replaces it, the page does not jump to the request: it shows the placeholder', () => {
    seed({ sessions: [waiting()] })
    render(<InboxPage />)
    expect(rows()[0]).toHaveAttribute('data-selected', 'true')
    act(() => seed({ sessions: [waiting()], pending: [req('r', { session_id: 'w' })] }))
    expect(screen.getByTestId('inbox-ghost')).toBeInTheDocument()
    expect(screen.queryByTestId('permission-card')).toBeNull()
  })
})
