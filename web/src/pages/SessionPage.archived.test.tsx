// 已归档会话的会话页（web.md § 4.6「输入区」）：daemon 对已归档的会话拒绝 `session.send`（`invalid` / `archived`），
// 所以输入框换成一条「已归档 · 取消归档」；点了调 `session.unarchive`，成功后换回输入框；`send` 被这样拒绝时给同样的说明。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Event, SessionSummary } from '@/protocol'
import i18n from '@/i18n'
import { ev, mockAgent, nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDrafts, useDrafts } from '@/stores/drafts'
import { useGit } from '@/stores/git'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
// 预览入口读 hub 的 /api/config（react-query）；这个测试没有 QueryClientProvider，也不涉及预览：当作 hub 没配预览
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => null }))
// 不真的去 daemon 拉会话：事件直接放进 store
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

const HINT = 'Archived · unarchive to send messages'

function seed({ summary: over = {}, events = [], conn = 'ready' }: { summary?: Partial<SessionSummary>; events?: Event[]; conn?: 'ready' | 'peer_offline' } = {}): void {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], conn }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, ...over }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, events.map((e) => ev(e)))
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
}
const sendCalls = () => rpc.calls.filter((c) => c.method === 'session.send')
const unarchiveCalls = () => rpc.calls.filter((c) => c.method === 'session.unarchive')
const toasts = () => useUi.getState().toasts.map((t) => t.text)

beforeEach(async () => {
  resetSeq()
  navigate.mockReset()
  await i18n.changeLanguage('en')
  rpc = new FakeRpc()
  rpc.handlers['session.send'] = () => ({ queued: false })
  rpc.handlers['session.unarchive'] = () => ({})
  setRpc(rpc)
  useUi.setState({ toasts: [] })
  useGit.setState({ bySession: {} })
  usePrefs.setState({ infoPanelOpen: false })
  resetDrafts()
})
afterEach(() => setRpc(null))

describe('SessionPage: an archived session', () => {
  it('swaps the input box for “Archived · Unarchive”; an active session keeps its input', async () => {
    seed({ summary: { archived: true } })
    const view = render(<SessionPage sessionId={SID} />)
    const bar = await screen.findByTestId('session-archived-bar')
    expect(within(bar).getByTestId('session-archived-hint')).toHaveTextContent(HINT)
    expect(within(bar).getByTestId('session-unarchive')).toBeEnabled()
    expect(within(bar).getByTestId('session-unarchive')).toHaveTextContent('Unarchive')
    expect(screen.queryByTestId('composer')).toBeNull()
    expect(screen.queryByTestId('composer-input')).toBeNull()
    view.unmount()

    seed({ summary: { archived: false } })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('composer-input')).toBeInTheDocument()
    expect(screen.queryByTestId('session-archived-bar')).toBeNull()
  })

  it('“Unarchive” calls session.unarchive, and on success the input is back — without waiting for a session.changed', async () => {
    const user = userEvent.setup()
    seed({ summary: { archived: true } })
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-unarchive'))
    await waitFor(() => expect(unarchiveCalls().map((c) => c.params)).toEqual([{ session_id: SID }]))
    expect(await screen.findByTestId('composer-input')).toBeInTheDocument()
    expect(screen.queryByTestId('session-archived-bar')).toBeNull()
    expect(useSessions.getState().list[SID]!.archived).toBe(false)
    // 恢复之后能发
    await user.type(screen.getByTestId('composer-input'), 'carry on')
    await user.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(sendCalls()).toHaveLength(1))
    expect(sendCalls()[0]!.params).toMatchObject({ session_id: SID, content: 'carry on' })
  })

  it('stays archived while the request is in flight (the button is busy), and the daemon’s session.changed also brings the input back', async () => {
    const user = userEvent.setup()
    let release: () => void = () => {}
    rpc.handlers['session.unarchive'] = () => new Promise((res) => (release = () => res({})))
    seed({ summary: { archived: true } })
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-unarchive'))
    await waitFor(() => expect(screen.getByTestId('session-unarchive')).toBeDisabled())
    expect(screen.queryByTestId('composer-input')).toBeNull()
    // 只点了一次，在途时再点也不会重复发
    await user.click(screen.getByTestId('session-unarchive'))
    expect(unarchiveCalls()).toHaveLength(1)
    release()
    await screen.findByTestId('composer-input')
    // 别的设备取消归档 → session.changed
    act(() => {
      useSessions.getState().applyChanged(NODE, { session_id: SID, title: 'hello', status: 'idle', archived: true, pending_permissions: 0, updated_at_ms: 9, last_seq: 0 })
    })
    expect(await screen.findByTestId('session-archived-bar')).toBeInTheDocument()
    act(() => {
      useSessions.getState().applyChanged(NODE, { session_id: SID, title: 'hello', status: 'idle', archived: false, pending_permissions: 0, updated_at_ms: 10, last_seq: 0 })
    })
    expect(await screen.findByTestId('composer-input')).toBeInTheDocument()
  })

  it('a failed unarchive keeps the bar, says why (a worktree that could not be rebuilt is not “git could not merge”), and can be tried again', async () => {
    const user = userEvent.setup()
    rpc.handlers['session.unarchive'] = () => {
      throw rpcError('internal', undefined, 'git_failed')
    }
    seed({ summary: { archived: true } })
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-unarchive'))
    await waitFor(() => expect(toasts()).toContain('Could not restore the session’s worktree, so it stays archived.'))
    expect(screen.getByTestId('session-archived-bar')).toBeInTheDocument()
    expect(screen.queryByTestId('composer-input')).toBeNull()
    expect(useSessions.getState().list[SID]!.archived).toBe(true)
    await waitFor(() => expect(screen.getByTestId('session-unarchive')).toBeEnabled())
    rpc.handlers['session.unarchive'] = () => ({})
    await user.click(screen.getByTestId('session-unarchive'))
    expect(await screen.findByTestId('composer-input')).toBeInTheDocument()
    expect(unarchiveCalls()).toHaveLength(2)
  })

  it('offline: the button is disabled (the request could not be sent anyway)', async () => {
    seed({ summary: { archived: true }, conn: 'peer_offline' })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('session-unarchive')).toBeDisabled()
  })

  it('says the same thing in Chinese', async () => {
    await i18n.changeLanguage('zh')
    seed({ summary: { archived: true } })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('session-archived-hint')).toHaveTextContent('已归档 · 取消归档后才能发送消息')
    expect(screen.getByTestId('session-unarchive')).toHaveTextContent('取消归档')
  })

  it('does not offer “Retry” on a recoverable error — that would send a message the daemon refuses', async () => {
    const errorEvent: Event = { type: 'error', code: 'agent_crashed', message: 'crashed', recoverable: true }
    seed({ summary: { archived: false }, events: [{ type: 'user_message', message_id: 'm1', content: 'go', by_device: 'me' }, errorEvent] })
    const view = render(<SessionPage sessionId={SID} />)
    expect(await screen.findByRole('button', { name: 'Retry' })).toBeInTheDocument() // 对照：没归档时有
    view.unmount()
    seed({ summary: { archived: true }, events: [{ type: 'user_message', message_id: 'm1', content: 'go', by_device: 'me' }, errorEvent] })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('session-archived-bar')
    expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull()
  })
})

describe('SessionPage: session.send refused with invalid / archived', () => {
  const archivedRefusal = () => {
    throw rpcError('invalid', undefined, 'archived')
  }

  it('shows the same explanation (toast + on the failed message), and the input becomes the “Archived · Unarchive” bar even though this page did not know yet', async () => {
    const user = userEvent.setup()
    rpc.handlers['session.send'] = archivedRefusal
    seed({ summary: { archived: false } }) // 别的设备刚归档，这一页还不知道
    render(<SessionPage sessionId={SID} />)
    await user.type(await screen.findByTestId('composer-input'), 'are you there')
    await user.click(screen.getByTestId('composer-send'))

    await waitFor(() => expect(toasts()).toContain(HINT))
    expect(toasts()).not.toContain('Failed') // 不是通用的失败提示
    const bar = await screen.findByTestId('session-archived-bar')
    expect(within(bar).getByTestId('session-archived-hint')).toHaveTextContent(HINT)
    expect(screen.queryByTestId('composer-input')).toBeNull()
    // 消息没丢：留在「失败」里，带同样的说明，取消归档之后可以重试
    const pending = useDrafts.getState().byKey[SID]!.unsent
    expect(pending).toMatchObject([{ content: 'are you there', state: 'failed', error: 'archived' }])
    const error = await screen.findByTestId('message-unsent-error')
    expect(error).toHaveAttribute('data-reason', 'archived')
    expect(error).toHaveTextContent(HINT)

    rpc.handlers['session.send'] = () => ({ queued: false })
    await userEvent.setup().click(within(bar).getByTestId('session-unarchive'))
    await screen.findByTestId('composer-input')
    await user.click(screen.getByTestId('message-retry'))
    await waitFor(() => expect(sendCalls()).toHaveLength(2))
    expect(sendCalls()[1]!.params).toMatchObject({ session_id: SID, content: 'are you there' })
  })

  it('an `invalid` for any other reason is the generic failure: the input stays', async () => {
    const user = userEvent.setup()
    rpc.handlers['session.send'] = () => {
      throw rpcError('invalid', undefined, 'worktree_not_active')
    }
    seed({ summary: { archived: false } })
    render(<SessionPage sessionId={SID} />)
    await user.type(await screen.findByTestId('composer-input'), 'hello')
    await user.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(toasts()).toContain('Failed'))
    expect(screen.queryByTestId('session-archived-bar')).toBeNull()
    expect(screen.getByTestId('composer-input')).toBeInTheDocument()
    expect(useSessions.getState().list[SID]!.archived).toBeFalsy()
  })

  it('“Retry” on an unsent message that the daemon now refuses as archived also ends in the “Archived · Unarchive” bar, and the message is not lost', async () => {
    const user = userEvent.setup()
    rpc.handlers['session.send'] = archivedRefusal
    seed({ summary: { archived: false } })
    useDrafts.setState((s) => ({
      loaded: { ...s.loaded, [SID]: true },
      byKey: { ...s.byKey, [SID]: { session_id: SID, text: '', unsent: [{ message_id: 'u1', content: 'queued while offline', created_at: 1, state: 'unsent' }] } },
    }))
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('message-retry'))
    await screen.findByTestId('session-archived-bar')
    expect(useDrafts.getState().byKey[SID]!.unsent.map((u) => u.message_id)).toEqual(['u1'])
  })
})

describe('SessionPage: archiving needs the session to be stopped or answered first', () => {
  const BLOCKED = 'Stop the session or answer its pending request first, then archive it.'
  const archiveCalls = () => rpc.calls.filter((c) => c.method === 'session.archive')

  beforeEach(() => {
    usePrefs.setState({ infoPanelOpen: true })
    rpc.handlers['session.archive'] = () => ({})
    rpc.handlers['session.cancel'] = () => ({})
  })

  it.each(['running', 'waiting_permission', 'starting', 'queued'] as const)('%s: the Archive button is off and says to stop or answer first; clicking sends nothing', async (status) => {
    const user = userEvent.setup()
    seed({ summary: { status } })
    render(<SessionPage sessionId={SID} />)
    const button = await screen.findByTestId('session-archive')
    expect(button).toBeDisabled()
    expect(button).toHaveAttribute('title', BLOCKED)
    expect(screen.getByTestId('session-archive-blocked')).toHaveTextContent(BLOCKED)
    expect(button).toHaveAttribute('aria-describedby', 'session-archive-blocked')
    await user.click(button)
    expect(archiveCalls()).toHaveLength(0)
  })

  it('an idle session archives as before, with no explanation lingering', async () => {
    const user = userEvent.setup()
    seed({ summary: { status: 'idle' } })
    render(<SessionPage sessionId={SID} />)
    const button = await screen.findByTestId('session-archive')
    expect(button).toBeEnabled()
    expect(screen.queryByTestId('session-archive-blocked')).toBeNull()
    await user.click(button)
    await waitFor(() => expect(archiveCalls().map((c) => c.params)).toEqual([{ session_id: SID }]))
  })

  it('the button comes back when the turn ends (status → idle)', async () => {
    seed({ summary: { status: 'running' } })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('session-archive')).toBeDisabled()
    act(() => {
      useSessions.getState().applyChanged(NODE, { session_id: SID, title: 'hello', status: 'idle', archived: false, pending_permissions: 0, updated_at_ms: 9, last_seq: 0 })
    })
    await waitFor(() => expect(screen.getByTestId('session-archive')).toBeEnabled())
    expect(screen.queryByTestId('session-archive-blocked')).toBeNull()
  })

  it('the explanation is in Chinese too', async () => {
    await i18n.changeLanguage('zh')
    seed({ summary: { status: 'waiting_permission' } })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('session-archive-blocked')).toHaveTextContent('先停止会话或答复待审批的请求，再归档。')
  })

  it('a `busy` from the daemon (this page’s status was stale) gets the same explanation, not the generic “agent is busy” wording', async () => {
    const user = userEvent.setup()
    rpc.handlers['session.archive'] = () => {
      throw rpcError('busy')
    }
    seed({ summary: { status: 'idle' } })
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-archive'))
    await waitFor(() => expect(toasts()).toContain(BLOCKED))
    expect(useSessions.getState().list[SID]!.archived).toBeFalsy()
  })

  it('un-archiving is never blocked by the status', async () => {
    seed({ summary: { status: 'running', archived: true } })
    render(<SessionPage sessionId={SID} />)
    const button = await screen.findByTestId('session-archive')
    expect(button).toBeEnabled()
    expect(button).toHaveTextContent('Unarchive')
    expect(screen.queryByTestId('session-archive-blocked')).toBeNull()
  })

  it('the archived bar keeps a Stop button while the session is somehow still running (a turn must not be unstoppable)', async () => {
    const user = userEvent.setup()
    seed({ summary: { status: 'running', archived: true } })
    render(<SessionPage sessionId={SID} />)
    const stop = await screen.findByTestId('session-archived-stop')
    expect(stop).toBeEnabled()
    expect(screen.getByTestId('session-unarchive')).toBeInTheDocument()
    await user.click(stop)
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'session.cancel').map((c) => c.params)).toEqual([{ session_id: SID }]))
  })

  it('no Stop button on an archived session that is idle, and none on a running session that is not archived (the composer has its own)', async () => {
    seed({ summary: { status: 'idle', archived: true } })
    const view = render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('session-archived-bar')
    expect(screen.queryByTestId('session-archived-stop')).toBeNull()
    view.unmount()
    seed({ summary: { status: 'running', archived: false } })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('composer-stop')).toBeInTheDocument()
    expect(screen.queryByTestId('session-archived-stop')).toBeNull()
  })
})
