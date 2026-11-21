// 从清单项派出的会话（`todo_id`）：顶栏的 chip 写着那一项的标题（清理过）、链接到它；右栏信息面板里也列出它；项没了就写明；
// 手动开的会话没有这些（spec/web.md § 4.6、§ 4.12）。
import { render, screen, waitFor } from '@testing-library/react'
import * as db from '@/db'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { SessionSummary } from '@/protocol'
import i18n from '@/i18n'
import { mockAgent, nodeEntry, resetSeq, summary, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDrafts } from '@/stores/drafts'
import { useGit } from '@/stores/git'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { resetTodosMemory } from '@/features/todos/service'
import { useTodos } from '@/features/todos/store'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => null }))
vi.mock('@/features/session/actions', async (orig) => ({ ...(await orig<object>()), openSession: vi.fn(async () => {}), closeSession: vi.fn(async () => {}) }))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

function seed(over: Partial<SessionSummary> = {}): void {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], info: { capabilities: { todo: true } } as never }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, ...over }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, [])
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
}
// 节点一就绪，清单会从 IndexedDB 把缓存读进内存（盖掉直接塞进 store 的东西）：所以也写进缓存
const putTodo = async (t: ReturnType<typeof todoItem>): Promise<void> => {
  await db.applyTodoCache(NODE, { put: [t] })
  useTodos.setState({ nodes: { [NODE]: { items: { [t.id]: t }, hydrated: true, synced: true, syncing: false, closed: { expanded: false, loading: false, complete: false, hasMore: false, failed: false } } } })
}

beforeEach(async () => {
  await db.resetDb()
  resetSeq()
  navigate.mockReset()
  await i18n.changeLanguage('en')
  rpc = new FakeRpc()
  rpc.handlers['session.mark_read'] = () => ({})
  setRpc(rpc)
  useUi.setState({ toasts: [] })
  useGit.setState({ bySession: {} })
  usePrefs.setState({ infoPanelOpen: true })
  resetDrafts()
  resetTodosMemory()
})
afterEach(() => setRpc(null))

describe('SessionPage: a session that came from a to-do', () => {
  it('shows a chip with the item’s title (cleaned to one line) linking to it, and lists the item in the info panel', async () => {
    await putTodo(todoItem({ id: 'td1', title: 'Fix the\nredirect‮' }))
    seed({ todo_id: 'td1' })
    render(<SessionPage sessionId={SID} />)
    await screen.findAllByTestId('session-todo-link')
    expect(screen.getAllByTestId('session-todo-link').length).toBeGreaterThanOrEqual(1)
    const link = screen.getAllByTestId('session-todo-link')[0]!
    expect(link).toHaveAttribute('href', expect.stringContaining('/todos/nnnnnnnn/td1'))
    expect(link.textContent).toContain('Fix the redirect')
    expect(link.textContent).not.toContain('‮')
    expect(screen.getByTestId('info-todo')).toBeInTheDocument()
  })

  it('an item that is closed (not in the open list) is asked for once from the node', async () => {
    rpc.handlers['todo.get'] = () => todoItem({ id: 'td1', title: 'Closed one', state: 'done' })
    seed({ todo_id: 'td1' })
    useTodos.setState({ nodes: { [NODE]: { items: {}, hydrated: true, synced: true, syncing: false, closed: { expanded: false, loading: false, complete: false, hasMore: false, failed: false } } } })
    render(<SessionPage sessionId={SID} />)
    await waitFor(() => expect(screen.getAllByTestId('session-todo-link')[0]!.textContent).toContain('Closed one'))
    // 顶栏的 chip 和信息面板同时要同一项：只问一次
    await new Promise((r) => setTimeout(r, 50))
    expect(rpc.calls.filter((c) => c.method === 'todo.get')).toHaveLength(1)
  })

  it('an item that was deleted is said to be deleted, with no link', async () => {
    rpc.handlers['todo.get'] = () => {
      throw rpcError('not_found')
    }
    seed({ todo_id: 'gone' })
    render(<SessionPage sessionId={SID} />)
    await waitFor(() => expect(screen.getAllByTestId('session-todo-deleted').length).toBeGreaterThan(0))
    expect(screen.queryByTestId('session-todo-link')).toBeNull()
  })

  it('a session that was opened by hand has no chip and no item in the panel', async () => {
    seed()
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('info-overview')
    expect(screen.queryByTestId('session-todo-link')).toBeNull()
    expect(screen.queryByTestId('info-todo')).toBeNull()
  })
})
