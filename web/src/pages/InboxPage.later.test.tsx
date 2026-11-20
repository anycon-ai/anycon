// 「待处理」页的「稍后」段（spec/web.md § 4.11）：agent 记的、还没人收下的清单项；收下 / 丢掉；不进导航数字、角标、标题；
// agent 的文字不可信（只当文本、去掉控制字符和双向控制符）。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { resetViewport } from '@/test/viewport'
import { setRpc } from '@/transport/instance'
import { inboxNowCount } from '@/features/inbox/derive'
import { getInboxSnapshot, inboxNowCountFromStores } from '@/features/inbox/model'
import { useInflight } from '@/features/session/inflight'
import { resetTodosMemory, syncTodos } from '@/features/todos/service'
import { openCount } from '@/features/todos/list'
import { useTodos, viewOf } from '@/features/todos/store'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
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
