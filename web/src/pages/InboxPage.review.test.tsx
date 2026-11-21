// 「待处理」页的「待验收」段（spec/web.md § 4.11）：派发过、`activity = review` 的清单项；行上有结果摘要和改动统计，动作是通过 / 再改改 / 打开改动；
// 数量只写在段标题上，不进导航数字、角标、标题；不自动选中、不自动展开；审批的安全规则（按 key 选中、占位）照旧。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { diffFile, nodeEntry, summary, todoItem, worktree } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { resetViewport, setViewport } from '@/test/viewport'
import { setRpc } from '@/transport/instance'
import { inboxNowCount } from '@/features/inbox/derive'
import { getInboxSnapshot, inboxNowCountFromStores } from '@/features/inbox/model'
import { useReviewUi } from '@/features/inbox/ui/ReviewParts'
import { useInflight } from '@/features/session/inflight'
import { useTodoHistory } from '@/features/todos/history'
import { handleTodoChanged, resetTodosMemory, syncTodos } from '@/features/todos/service'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/inbox' }))

import { InboxPage } from './InboxPage'

const A = 'a'.repeat(32)
const info = { capabilities: { todo: true } } as unknown as NodeInfo
let rpc: FakeRpc
let daemon: FakeTodoDaemon
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 5))
  })
}
const result = (text: string) => ({ seq: 2, ts_ms: 2000, kind: 'result', session_id: 's1', turn: 1, text, truncated: false }) as never
const reviewItem = (id: string, over: Partial<Parameters<typeof todoItem>[0]> = {}) => todoItem({ id, title: `review ${id}`, activity: 'review' as never, sessions: ['s1'] as never, updated_ms: 100, ...over })
const reviewRows = (): HTMLElement[] => within(screen.getByTestId('inbox-section-review')).queryAllByTestId('inbox-row')

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
  rpc.handlers['todo.history'] = () => ({ events: [result('Fixed the redirect.\nSecond line.')], has_more: false })
  rpc.handlers['session.diff'] = () => ({ from_sha: 'a'.repeat(40), files: [diffFile({ path: 'a.ts', additions: 5, deletions: 2 }), diffFile({ path: 'b.ts', additions: 1, deletions: 0 })] })
  rpc.handlers['session.send'] = () => ({ queued: false })
  resetViewport()
  useInflight.setState({ ids: new Set(), handled: new Set() })
  usePrefs.setState({ sidebarNodes: null })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null })
  useSessions.setState({ list: { s1: { ...summary({ id: 's1', status: 'idle', worktree: worktree() }), node_id: A, cached_seq: 0 } as never }, pending: {}, open: {} })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info, conn: 'ready' }) }, order: [A], loaded: true })
  useTodoHistory.getState().reset()
  useReviewUi.setState({ reworkKey: null })
  resetTodosMemory()
  useUi.setState({ toasts: [] })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('To review section', () => {
  it('lists the items in review with the count on the header, between “now” and “later”; the rows show the result summary and the change counts', async () => {
    await load(reviewItem('r1'), reviewItem('r2', { activity: 'working' as never }), todoItem({ id: 'plain', title: 'plain' }))
    expect(reviewRows()).toHaveLength(1)
    expect(screen.getByTestId('inbox-count-review')).toHaveTextContent('1')
    const brief = await screen.findByTestId('inbox-review-brief')
    expect(brief).toHaveTextContent('Fixed the redirect.')
    expect(brief).toHaveTextContent('2 files changed')
    expect(brief).toHaveTextContent('+6 −2')
    expect(brief).not.toHaveTextContent('Second line')
    const ids = [...document.querySelectorAll('[data-testid^="inbox-section-"]')].map((n) => n.getAttribute('data-testid'))
    expect(ids).toEqual(['inbox-section-review'])
  })

  it('is not counted in the now count, the badge or the title: only the section header has the number', async () => {
    daemon.seed(reviewItem('r1'), reviewItem('r2'))
    await syncTodos(A)
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(0)
    expect(inboxNowCountFromStores()).toBe(0)
  })

  it('is never auto-selected or auto-expanded: not on a wide screen, not on a phone', async () => {
    await load(reviewItem('r1'))
    expect(screen.queryByTestId('inbox-detail')).toBeNull()
    expect(screen.queryByTestId('inbox-review-detail')).toBeNull()
    expect(rpc.calls.some((c) => c.method === 'session.send')).toBe(false)
  })

  it('on a phone nothing is expanded by default either', async () => {
    setViewport('phone')
    await load(reviewItem('r1'))
    expect(screen.queryByTestId('inbox-inline-detail')).toBeNull()
  })

  it('clicking a row shows the conclusion (sanitised Markdown) and the three actions plus the review page link', async () => {
    const user = userEvent.setup()
    rpc.handlers['todo.history'] = () => ({ events: [result('Done. <script>alert(1)</script> **bold**')], has_more: false })
    await load(reviewItem('r1'))
    await user.click(screen.getByTestId('inbox-row-main'))
    const box = await screen.findByTestId('inbox-review-result')
    expect(box).toHaveTextContent('Done.')
    expect(box.querySelector('script')).toBeNull()
    expect(screen.getByTestId('inbox-detail-approve')).toBeEnabled()
    expect(screen.getByTestId('inbox-detail-rework')).toBeEnabled()
    expect(screen.getByTestId('inbox-detail-open-changes')).toHaveAttribute('href', expect.stringContaining('/s/s1'))
    expect(screen.getByTestId('inbox-detail-review-page')).toHaveAttribute('href', expect.stringContaining('/review'))
  })
})

describe('the actions', () => {
  it('Approve on the row sets the state to done and the row leaves', async () => {
    const user = userEvent.setup()
    await load(reviewItem('r1'))
    await user.click(screen.getByTestId('inbox-approve'))
    await waitFor(() => expect(daemon.todos.get('r1')!.state).toBe('done'))
    await flush()
    expect(screen.queryByTestId('inbox-section-review')).toBeNull()
  })

  it('Needs more work on the row selects it and opens the input; sending goes to the latest linked session only, and an empty text sends nothing', async () => {
    const user = userEvent.setup()
    await load(reviewItem('r1'))
    await user.click(screen.getByTestId('inbox-rework'))
    expect(await screen.findByTestId('inbox-rework-box')).toBeInTheDocument()
    expect(screen.getByTestId('inbox-rework-send')).toBeDisabled()
    await user.type(screen.getByTestId('inbox-rework-input'), 'handle the empty case')
    await user.click(screen.getByTestId('inbox-rework-send'))
    await waitFor(() => expect(rpc.calls.find((c) => c.method === 'session.send')?.params).toMatchObject({ session_id: 's1', content: 'handle the empty case' }))
    // 状态没改：activity 由 daemon 随会话变回处理中
    expect(daemon.todos.get('r1')!.state).toBe('open')
  })

  it('Open the changes links to the session’s changes view', async () => {
    await load(reviewItem('r1'))
    expect(screen.getByTestId('inbox-open-changes')).toHaveAttribute('href', expect.stringContaining('/s/s1'))
  })

  it('the actions are disabled while the node is offline', async () => {
    useNodes.getState().patch(A, { conn: 'peer_offline' })
    await load(reviewItem('r1'))
    expect(screen.getByTestId('inbox-approve')).toBeDisabled()
    expect(screen.getByTestId('inbox-rework')).toBeDisabled()
  })
})

describe('selection stays safe', () => {
  it('an item that leaves because another device approved it leaves a placeholder, it does not hand the selection to the next item', async () => {
    const user = userEvent.setup()
    await load(reviewItem('r1'), reviewItem('r2', { updated_ms: 200 }))
    await user.click(reviewRows()[0]!.querySelector('[data-testid="inbox-row-main"]')!)
    await screen.findByTestId('inbox-review-detail')
    act(() => handleTodoChanged(A, { id: 'r1', todo: todoItem({ id: 'r1', title: 'review r1', state: 'done', activity: undefined, sessions: ['s1'] as never, updated_ms: 300 }) }))
    await flush()
    expect(screen.getByTestId('inbox-ghost')).toBeInTheDocument()
    expect(screen.queryByTestId('inbox-review-detail')).toBeNull()
  })

  it('approving here advances only within the review section (never into the next kind of item)', async () => {
    const user = userEvent.setup()
    await load(reviewItem('r1', { updated_ms: 100 }), reviewItem('r2', { updated_ms: 200 }))
    await user.click(reviewRows()[0]!.querySelector('[data-testid="inbox-row-main"]')!)
    await user.click(await screen.findByTestId('inbox-detail-approve'))
    await waitFor(() => expect(reviewRows()).toHaveLength(1))
    // 落到同一段里的下一条
    await waitFor(() => expect(screen.getByTestId('inbox-review-detail')).toBeInTheDocument())
  })
})
