// 「待处理」页的「稍后」段（spec/web.md § 4.11）：agent 记的、还没人收下的清单项；收下 / 丢掉；不进导航数字、角标、标题；
// agent 的文字不可信（只当文本、去掉控制字符和双向控制符）。
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, summary, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { resetViewport, setViewport } from '@/test/viewport'
import { setRpc } from '@/transport/instance'
import { inboxNowCount } from '@/features/inbox/derive'
import { getInboxSnapshot, inboxNowCountFromStores } from '@/features/inbox/model'
import { useInflight } from '@/features/session/inflight'
import { handleTodoChanged, resetTodosMemory, syncTodos } from '@/features/todos/service'
import { openCount } from '@/features/todos/list'
import { useTodos, viewOf } from '@/features/todos/store'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type PendingPermission } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/inbox' }))

import { InboxPage } from './InboxPage'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const info = (todo: boolean): NodeInfo => ({ capabilities: { todo } }) as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeTodoDaemon
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 5))
  })
}
const agent = (id: string, over: Partial<Parameters<typeof todoItem>[0]> = {}) => todoItem({ id, title: `agent ${id}`, source: { kind: 'agent', session_id: 's1' }, unreviewed: true, ...over })
const laterRows = (): HTMLElement[] => within(screen.getByTestId('inbox-section-later')).queryAllByTestId('inbox-row')

async function load(...items: Array<ReturnType<typeof todoItem>>): Promise<void> {
  daemon.seed(...items)
  await syncTodos(A)
  render(<InboxPage />)
  await flush()
}

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  resetViewport()
  useInflight.setState({ ids: new Set(), handled: new Set() })
  usePrefs.setState({ sidebarNodes: null })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(true), conn: 'ready' }), [B]: nodeEntry({ id: B, name: 'beta', info: info(false), conn: 'ready' }) }, order: [A, B], loaded: true })
  resetTodosMemory()
  useUi.setState({ toasts: [] })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('Later section', () => {
  it('lists unreviewed agent items with a count on the header; reviewed ones and the user’s own are not there', async () => {
    await load(agent('u1'), agent('u2'), agent('seen', { unreviewed: false }), todoItem({ id: 'mine', title: 'mine' }), agent('done', { state: 'done' }))
    expect(laterRows()).toHaveLength(2)
    expect(screen.getByTestId('inbox-count-later')).toHaveTextContent('2')
  })

  it('does not change the “now” count, the nav number inputs, or the badge', async () => {
    useTodos.setState({ outbox: [] })
    daemon.seed(agent('u1'), agent('u2'))
    await syncTodos(A)
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(0)
    expect(inboxNowCountFromStores()).toBe(0)
    expect(openCount(viewOf(useTodos.getState(), [A]))).toBe(0)
    render(<InboxPage />)
    await flush()
    expect(screen.getByTestId('inbox-count-later')).toHaveTextContent('2')
    expect(document.title).not.toMatch(/\(\d+\)/)
  })

  it('Accept marks it reviewed: it leaves the section and joins the to-do list count', async () => {
    const user = userEvent.setup()
    await load(agent('u1'))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-accept'))
    await flush()
    expect(daemon.todos.get('u1')!.unreviewed).toBe(false)
    expect(daemon.todos.get('u1')!.state).toBe('open')
    await waitFor(() => expect(screen.queryByTestId('inbox-section-later')).toBeNull())
    expect(openCount(viewOf(useTodos.getState(), [A]))).toBe(1)
  })

  it('Dismiss drops it (state dropped, not deleted)', async () => {
    const user = userEvent.setup()
    await load(agent('u1'))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-dismiss'))
    await flush()
    expect(daemon.todos.get('u1')).toMatchObject({ state: 'dropped' })
    expect(openCount(viewOf(useTodos.getState(), [A]))).toBe(0)
  })

  it('only shows the nodes picked in the node filter', async () => {
    usePrefs.setState({ sidebarNodes: [B] })
    await load(agent('u1'))
    expect(screen.queryByTestId('inbox-section-later')).toBeNull()
  })

  it('agent text is untrusted: row and detail show it as text, control and bidi characters removed', async () => {
    const user = userEvent.setup()
    await load(agent('u1', { title: 'ship‮this\u0007 <b>now</b>', body: '**hi**\n\n<script>alert(1)</script>\n\n[x](javascript:alert(2))' }))
    const main = within(laterRows()[0]!).getByTestId('inbox-row-main')
    expect(main.textContent).not.toContain('\u202E')
    expect(main.textContent).not.toContain('\u0007')
    expect(main.querySelector('b')).toBeNull()
    await user.click(main)
    await flush()
    const body = await screen.findByTestId('inbox-later-body')
    expect(body.querySelector('script')).toBeNull()
    expect(body.querySelector('a[href^="javascript:"]')).toBeNull()
    expect(body.querySelector('strong')).toHaveTextContent('hi')
  })
})

describe('Later items are never selected or expanded on the user’s behalf (U9)', () => {
  const permission = (): PendingPermission => ({
    request_id: 'r1',
    session_id: 's-r1',
    node_id: A,
    kind: 'tool',
    title: 'Run r1',
    detail: { tool: { tool: 'Bash', category: 'shell', input: { command: 'echo r1' }, cwd: '/work/r1', detail: { command: 'echo r1' } } },
    options: [
      { id: 'allow', label: 'Allow once', kind: 'allow_once' },
      { id: 'deny', label: 'Deny once', kind: 'reject_once' },
    ],
    created_at_ms: Date.now() - 120_000,
    expires_at_ms: null,
  })
  const seedPermission = (): void => useSessions.setState({ list: { 's-r1': { ...summary({ id: 's-r1', title: 's-r1', project: 'app' }), node_id: A, cached_seq: 0 } }, pending: { r1: permission() }, open: {} })
  const press = (key: string): void => void fireEvent.keyDown(window, { key })

  it('desktop, “Now” empty: no Later item is selected and no detail is shown until the user picks one', async () => {
    await load(agent('u1'))
    expect(screen.queryByTestId('inbox-detail')).toBeNull()
    expect(document.querySelector('[aria-current="true"]')).toBeNull()
    await userEvent.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    expect(await screen.findByTestId('inbox-detail')).toHaveAttribute('data-kind', 'later')
  })

  it('phone, “Now” empty: the first Later item is not expanded by default', async () => {
    setViewport('phone')
    await load(agent('u1'))
    expect(screen.queryByTestId('inbox-inline-detail')).toBeNull()
  })

  it('with a request in “Now”, the default selection is the request, never a Later item', async () => {
    seedPermission()
    await load(agent('u1'))
    expect(await screen.findByTestId('permission-card')).toBeInTheDocument()
    expect(laterRows()[0]!.querySelector('[aria-current="true"]')).toBeNull()
  })

  it('J / K walk the list in section order: Now first, then Later', async () => {
    seedPermission()
    await load(agent('u1'), agent('u2', { updated_ms: 2000, created_ms: 2000 }))
    await screen.findByTestId('permission-card')
    const order = (): string[] => [...document.querySelectorAll('[data-inbox-row]')].map((r) => r.getAttribute('data-inbox-row')!)
    expect(order()[0]).toMatch(/^request:/)
    expect(order().slice(1).every((k) => k.startsWith('later:'))).toBe(true)
    press('j')
    await waitFor(() => expect(document.querySelector('[data-inbox-row] [aria-current="true"]')?.closest('[data-inbox-row]')?.getAttribute('data-inbox-row')).toBe(order()[1]))
    press('j')
    await waitFor(() => expect(document.querySelector('[data-inbox-row] [aria-current="true"]')?.closest('[data-inbox-row]')?.getAttribute('data-inbox-row')).toBe(order()[2]))
    press('k')
    await waitFor(() => expect(document.querySelector('[data-inbox-row] [aria-current="true"]')?.closest('[data-inbox-row]')?.getAttribute('data-inbox-row')).toBe(order()[1]))
  })

  it('accepting the last Later item while a request waits in “Now”: nothing is selected for the user (no jump into the request), and when they open the request its buttons are locked for 600 ms', async () => {
    const user = userEvent.setup()
    seedPermission()
    await load(agent('u1'))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    expect(await screen.findByTestId('inbox-detail')).toHaveAttribute('data-kind', 'later')
    await user.click(within(screen.getByTestId('inbox-detail')).queryByTestId('inbox-accept') ?? within(laterRows()[0]!).getByTestId('inbox-accept'))
    await flush()
    await waitFor(() => expect(screen.queryByTestId('inbox-section-later')).toBeNull())
    expect(screen.queryByTestId('permission-card')).toBeNull()
    expect(document.querySelector('[aria-current="true"]')).toBeNull()
    await user.click(within(screen.getByTestId('inbox-section-now')).getByTestId('inbox-row-main'))
    const card = await screen.findByTestId('permission-card')
    const allow = within(card).getByTestId('permission-option-allow')
    expect(allow).toBeDisabled()
    await waitFor(() => expect(allow).toBeEnabled(), { timeout: 2000 })
  })

  it('when another device accepts the item you are looking at, a placeholder stays in the Later section (not at the bottom of “Now”), and nothing else is selected for you', async () => {
    setViewport('phone')
    seedPermission()
    await load(agent('u1'))
    await userEvent.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    await screen.findByTestId('inbox-inline-detail')
    await act(async () => handleTodoChanged(A, { id: 'u1', todo: { ...daemon.todos.get('u1')!, unreviewed: false, updated_ms: 99_000 } }))
    await flush()
    const ghost = await screen.findByTestId('inbox-ghost')
    expect(screen.getByTestId('inbox-section-later')).toContainElement(ghost)
    expect(within(screen.getByTestId('inbox-section-now')).queryByTestId('inbox-ghost')).toBeNull()
  })

  it('Accept and Dismiss are disabled while the node is offline', async () => {
    await load(agent('u1'))
    useNodes.getState().patch(A, { conn: 'peer_offline' })
    await flush()
    const row = laterRows()[0]!
    expect(within(row).getByTestId('inbox-accept')).toBeDisabled()
    expect(within(row).getByTestId('inbox-dismiss')).toBeDisabled()
  })

  it('a hidden page (a phone tab in the background) selects nothing and ignores keys', async () => {
    daemon.seed(agent('u1'))
    await syncTodos(A)
    render(<InboxPage active={false} />)
    await flush()
    press('j')
    press('Enter')
    expect(screen.queryByTestId('inbox-detail')).toBeNull()
    expect(document.querySelector('[aria-current="true"]')).toBeNull()
  })

  it('U8: the detail links back to the session it came from when it still exists, and says so when it does not', async () => {
    const user = userEvent.setup()
    await load(agent('u1'))
    await user.click(within(laterRows()[0]!).getByTestId('inbox-row-main'))
    expect(await screen.findByTestId('inbox-later-source')).toHaveTextContent(/no longer exists/i)
    useSessions.setState({ list: { s1: { ...summary({ id: 's1', title: 'origin' }), node_id: A, cached_seq: 0 } } })
    await flush()
    expect(screen.getByTestId('inbox-later-open-session').getAttribute('href')).toMatch(/\/n\/aaaaaaaa\/s\/s1$/)
  })
})
