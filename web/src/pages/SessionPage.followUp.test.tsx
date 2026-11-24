// 会话页上的「完成后提醒」（web.md § 4.6.1）：顶栏开关、输入区上方的条、输入框旁的小开关，以及已读上报。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Event, SessionSummary } from '@/protocol'
import i18n from '@/i18n'
import { ev, mockAgent, nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { RpcCallError } from '@/transport/rpcClient'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDrafts } from '@/stores/drafts'
import { useGit } from '@/stores/git'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => null }))
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

function seed({ summary: over = {}, events = [] }: { summary?: Partial<SessionSummary>; events?: Event[] } = {}): void {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()] }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, ...over }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, events.map((e) => ev(e)))
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
}
const calls = (method: string) => rpc.calls.filter((c) => c.method === method).map((c) => c.params)

beforeEach(async () => {
  resetSeq()
  navigate.mockReset()
  await i18n.changeLanguage('en')
  rpc = new FakeRpc()
  rpc.handlers['session.set_marks'] = () => ({})
  rpc.handlers['session.mark_read'] = () => ({})
  setRpc(rpc)
  useUi.setState({ toasts: [] })
  useGit.setState({ bySession: {} })
  usePrefs.setState({ infoPanelOpen: false })
  resetDrafts()
})
afterEach(() => setRpc(null))

describe('SessionPage: follow-up', () => {
  it('the header flag marks the session, and the bar above the input says the next turn will notify', async () => {
    const user = userEvent.setup()
    seed()
    render(<SessionPage sessionId={SID} />)
    const toggle = await screen.findByTestId('follow-up-toggle')
    expect(toggle).toHaveAttribute('aria-pressed', 'false')
    expect(screen.queryByTestId('follow-up-bar')).toBeNull()
    await user.click(toggle)
    await waitFor(() => expect(calls('session.set_marks')).toEqual([{ session_id: SID, follow_up: true }]))
    const bar = await screen.findByTestId('follow-up-bar')
    expect(bar).toHaveAttribute('data-running', 'false')
    expect(bar).toHaveTextContent('You will be notified when the next turn finishes.')
    expect(screen.getByTestId('follow-up-toggle')).toHaveAttribute('aria-pressed', 'true')
  })

  it('“Cancel reminder” clears the mark', async () => {
    const user = userEvent.setup()
    seed({ summary: { follow_up: true } })
    render(<SessionPage sessionId={SID} />)
    const clear = await screen.findByTestId('follow-up-clear')
    expect(clear).toHaveTextContent('Cancel reminder')
    await user.click(clear)
    await waitFor(() => expect(calls('session.set_marks')).toEqual([{ session_id: SID, follow_up: false }]))
    await waitFor(() => expect(screen.queryByTestId('follow-up-bar')).toBeNull())
  })

  it('while the session runs the bar says it will notify when it finishes; there is no “ready” state and no “Mark done”', async () => {
    seed({ summary: { follow_up: true, status: 'running' } })
    render(<SessionPage sessionId={SID} />)
    const bar = await screen.findByTestId('follow-up-bar')
    expect(bar).toHaveAttribute('data-running', 'true')
    expect(bar).toHaveTextContent('when this session finishes')
    act(() => useSessions.setState((s) => ({ list: { ...s.list, [SID]: { ...s.list[SID]!, status: 'idle' } } })))
    await waitFor(() => expect(screen.getByTestId('follow-up-bar')).toHaveAttribute('data-running', 'false'))
    expect(screen.queryByText('Mark done')).toBeNull()
    expect(calls('session.set_marks')).toEqual([])
  })

  it('an idle session with background tasks still running says it will notify when it finishes, not that it is waiting for you', async () => {
    seed({ summary: { follow_up: true, status: 'idle', background_tasks: 2 } })
    render(<SessionPage sessionId={SID} />)
    const bar = await screen.findByTestId('follow-up-bar')
    expect(bar).toHaveAttribute('data-running', 'true')
    expect(bar).toHaveTextContent('You will be notified when this session finishes.')
    expect(bar).not.toHaveTextContent('waiting for you')
    expect(screen.queryByText('Mark done')).toBeNull()
    expect(screen.getByTestId('follow-up-clear')).toHaveTextContent('Cancel reminder')
  })

  it('opening a marked session does not clear the mark', async () => {
    seed({ summary: { follow_up: true } })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('follow-up-bar')
    expect(calls('session.set_marks')).toEqual([])
    expect(useSessions.getState().list[SID]!.follow_up).toBe(true)
  })

  it('an unmarked session offers “Notify when done” beside the input (wide screens), which marks it and then gives way to the bar', async () => {
    const user = userEvent.setup()
    seed()
    render(<SessionPage sessionId={SID} />)
    const chip = await screen.findByTestId('follow-up-chip')
    expect(screen.getByTestId('composer-aside')).toContainElement(chip)
    await user.click(chip)
    await waitFor(() => expect(calls('session.set_marks')).toEqual([{ session_id: SID, follow_up: true }]))
    await waitFor(() => expect(screen.queryByTestId('follow-up-chip')).toBeNull())
    expect(screen.getByTestId('follow-up-bar')).toBeInTheDocument()
  })

  it.each([
    ['an old daemon that does not know the method (JSON-RPC -32601)', () => new RpcCallError('rpc', 'method not found', { code: -32601, message: 'method not found: session.set_marks', data: { code: 'unsupported' } })],
    ['a daemon that answers unsupported', () => rpcError('unsupported')],
  ])('says the daemon is too old (not that the agent cannot) when %s rejects the mark, from the flag and from the chip', async (_name, make) => {
    const user = userEvent.setup()
    rpc.handlers['session.set_marks'] = () => {
      throw make()
    }
    seed()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('follow-up-toggle'))
    await waitFor(() => expect(useUi.getState().toasts.map((x) => x.text)).toEqual(["This node's daemon is too old. Update it to use this."]))
    useUi.setState({ toasts: [] })
    await user.click(await screen.findByTestId('follow-up-chip'))
    await waitFor(() => expect(useUi.getState().toasts.map((x) => x.text)).toEqual(["This node's daemon is too old. Update it to use this."]))
    // 本地标记已改回去
    expect(useSessions.getState().list[SID]!.follow_up).toBeFalsy()
  })

  it('other mark failures keep their ordinary wording', async () => {
    const user = userEvent.setup()
    rpc.handlers['session.set_marks'] = () => {
      throw rpcError('not_found')
    }
    seed()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('follow-up-toggle'))
    await waitFor(() => expect(useUi.getState().toasts.map((x) => x.text)).toEqual(['Not found.']))
  })

  it('on a narrow screen there is no chip beside the input; the header flag is still there', async () => {
    const mm = vi.spyOn(window, 'matchMedia').mockImplementation(
      (query: string) => ({ matches: false, media: query, onchange: null, addEventListener: () => {}, removeEventListener: () => {}, addListener: () => {}, removeListener: () => {}, dispatchEvent: () => false }) as unknown as MediaQueryList,
    )
    try {
      seed()
      render(<SessionPage sessionId={SID} />)
      expect(await screen.findByTestId('follow-up-toggle')).toBeInTheDocument()
      expect(screen.queryByTestId('follow-up-chip')).toBeNull()
    } finally {
      mm.mockRestore()
    }
  })

  it('has no bar and no toggle on an archived session’s input area', async () => {
    seed({ summary: { follow_up: true, archived: true } })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('session-archived-bar')
    expect(screen.queryByTestId('follow-up-bar')).toBeNull()
  })

  it('reports what it has shown once the session is stopped, so the unread dot goes away', async () => {
    seed({ summary: { status: 'idle', last_seq: 5, read_seq: 1 }, events: [{ type: 'text_delta', text: 'a' }, { type: 'text_delta', text: 'b' }, { type: 'turn_done', turn_id: 't1', reason: 'completed', usage: undefined } as unknown as Event] })
    render(<SessionPage sessionId={SID} />)
    await waitFor(() => expect(calls('session.mark_read').length).toBeGreaterThan(0))
    expect(useSessions.getState().list[SID]!.read_seq).toBeGreaterThan(1)
  })
})
