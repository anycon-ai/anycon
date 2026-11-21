// 左栏：从清单项派出的会话在项目下面收成一行「来自清单 N」（可展开）；手动开的会话照旧；外观设置里可以关掉折叠（spec/web.md § 3）。
import type { ReactNode } from 'react'
import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mockAgent, nodeEntry, project, resetSeq, summary } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type SessionItem } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))
vi.mock('@/api/queries', () => ({ useMe: () => ({ data: undefined }), useDevices: () => ({ data: undefined }), logout: vi.fn() }))

import { Sidebar } from './Sidebar'

const NODE = 'n'.repeat(32)
const wrap = (ui: ReactNode) => render(<QueryClientProvider client={new QueryClient()}>{ui}</QueryClientProvider>)
const row = (over: Parameters<typeof summary>[0]): SessionItem => ({ ...summary({ project: 'app', ...over }), node_id: NODE, cached_seq: 0 })
const seed = (...items: SessionItem[]): void => useSessions.setState({ list: Object.fromEntries(items.map((i) => [i.id, i])), pending: {}, open: {} })

beforeEach(() => {
  resetSeq()
  usePrefs.setState({ sidebarOpen: true, sidebarNodes: null, sidebarRecent: false, sidebarFoldTodo: true })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects: [project({ name: 'app' })] }) }, order: [NODE], loaded: true })
})

describe('sidebar: sessions from the to-do list', () => {
  it('collapse into one “From the to-do list N” row under their project; hand-started sessions stay as they are', async () => {
    const user = userEvent.setup()
    seed(row({ id: 'a', title: 'by hand', status: 'idle' }), row({ id: 'b', title: 'from todo one', todo_id: 't1', status: 'idle' }), row({ id: 'c', title: 'from todo two', todo_id: 't2', status: 'idle' }))
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByText('by hand')).toBeInTheDocument()
    const fold = screen.getByTestId('sidebar-todo-fold')
    expect(fold).toHaveAttribute('data-count', '2')
    expect(within(fold).getByTestId('sidebar-todo-fold-toggle')).toHaveTextContent('From the to-do list: 2')
    expect(screen.queryByText('from todo one')).toBeNull()
    await user.click(screen.getByTestId('sidebar-todo-fold-toggle'))
    expect(screen.getByText('from todo one')).toBeInTheDocument()
    expect(screen.getByText('from todo two')).toBeInTheDocument()
    await user.click(screen.getByTestId('sidebar-todo-fold-toggle'))
    expect(screen.queryByText('from todo one')).toBeNull()
  })

  it('the fold says when something inside is running or waiting for approval (the sessions that need you are also in the Inbox)', () => {
    seed(row({ id: 'b', title: 'running one', todo_id: 't1', status: 'running' }), row({ id: 'c', title: 'asks', todo_id: 't2', status: 'waiting_permission', pending_permissions: 2 }))
    wrap(<Sidebar nodeId={NODE} />)
    const toggle = screen.getByTestId('sidebar-todo-fold-toggle')
    expect(within(toggle).getByRole('img', { name: 'Running' })).toBeInTheDocument()
    expect(toggle).toHaveTextContent('2')
  })

  it('a project with no to-do sessions has no fold row', () => {
    seed(row({ id: 'a', title: 'by hand', status: 'idle' }))
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-todo-fold')).toBeNull()
  })

  it('the appearance setting turns the folding off: every session is listed as before', () => {
    usePrefs.setState({ sidebarFoldTodo: false })
    seed(row({ id: 'b', title: 'from todo one', todo_id: 't1', status: 'idle' }))
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-todo-fold')).toBeNull()
    expect(screen.getByText('from todo one')).toBeInTheDocument()
  })
})
