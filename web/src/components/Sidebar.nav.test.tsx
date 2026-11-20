// 桌面左栏顶部的导航（spec/web.md § 3.1）：「待处理」带「现在」的个数，在会话树上面；节点菜单里有「节点首页」。
import type { ReactNode } from 'react'
import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mockAgent, nodeEntry, project, resetSeq, summary } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type SessionItem } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
const route = vi.hoisted(() => ({ pathname: '/' }))
vi.mock('@tanstack/react-router', async (orig) => {
  const m = mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' })
  return { ...m, useRouterState: (o?: { select?: (s: unknown) => unknown }) => (o?.select ? o.select({ location: { pathname: route.pathname } }) : null) }
})
vi.mock('@/api/queries', () => ({ useMe: () => ({ data: undefined }), useDevices: () => ({ data: undefined }), logout: vi.fn() }))

import { Sidebar } from './Sidebar'

const NODE = 'n'.repeat(32)
const wrap = (ui: ReactNode) => render(<QueryClientProvider client={new QueryClient()}>{ui}</QueryClientProvider>)
const row = (over: Parameters<typeof summary>[0]): SessionItem => ({ ...summary({ project: 'app', ...over }), node_id: NODE, cached_seq: 0 })

beforeEach(() => {
  resetSeq()
  navigate.mockReset()
  route.pathname = '/'
  usePrefs.setState({ sidebarOpen: true, sidebarNodes: null, sidebarRecent: true })
  useEnroll.setState({ items: {}, resolved: {} })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'alpha', agents: [mockAgent()], projects: [project({ name: 'app' })] }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
})

describe('sidebar navigation', () => {
  it('has an Inbox link above the session tree, with no number when nothing waits', () => {
    wrap(<Sidebar nodeId={NODE} />)
    const nav = screen.getByTestId('sidebar-nav')
    const link = within(nav).getByTestId('sidebar-inbox')
    expect(link).toHaveAttribute('href', '/inbox')
    expect(link).toHaveTextContent('Inbox')
    expect(screen.queryByTestId('sidebar-inbox-count')).toBeNull()
    const tree = screen.getByRole('navigation', { name: 'Sessions' })
    expect(nav.compareDocumentPosition(tree) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
  })

  it('shows the “Now” count: approvals, stopped follow-ups, errors and device requests', () => {
    useSessions.setState({ list: { f: row({ id: 'f', follow_up: true, status: 'idle' }), e: row({ id: 'e', status: 'error', updated_at_ms: Date.now() }) } })
    useSessions.getState().addPending(NODE, { request_id: 'r', session_id: 'x', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    useEnroll.getState().upsert(NODE, { enroll_id: 'en', expires_at_ms: Date.now() + 60_000, created_at_ms: Date.now(), status: 'wait_approval', method: 'code', approval: true })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-inbox-count')).toHaveTextContent('4')
    expect(screen.getByTestId('sidebar-inbox')).toHaveAccessibleName('Inbox, 4 waiting')
  })

  it('does not count unread sessions or sessions that are still running', () => {
    useSessions.setState({ list: { u: row({ id: 'u', last_seq: 9, read_seq: 1 }), r: row({ id: 'r', status: 'running', follow_up: true }) } })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-inbox-count')).toBeNull()
  })

  it('only counts the nodes ticked in the node filter', () => {
    const other = 'o'.repeat(32)
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'alpha' }), [other]: nodeEntry({ id: other, name: 'beta' }) }, order: [NODE, other], loaded: true })
    useSessions.getState().addPending(NODE, { request_id: 'r1', session_id: 'x', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    useSessions.getState().addPending(other, { request_id: 'r2', session_id: 'y', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    usePrefs.setState({ sidebarNodes: [NODE] })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-inbox-count')).toHaveTextContent('1')
  })

  it('marks the inbox as the current page when it is open', () => {
    route.pathname = '/inbox'
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-inbox')).toHaveAttribute('aria-current', 'page')
  })

  it('the Logo still goes to “/” (which redirects to the inbox)', () => {
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getAllByRole('link').find((a) => a.getAttribute('href') === '/')).toBeTruthy()
  })
})

describe('node menu', () => {
  it('has “Node home”, the way back to the old home page, next to “Node settings”', async () => {
    const user = userEvent.setup()
    wrap(<Sidebar nodeId={NODE} />)
    await user.click(screen.getByTestId('node-switcher'))
    await user.click(await screen.findByTestId('node-home'))
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node', params: { node: 'nnnnnnnn' } })
  })
})

describe('node filter dot', () => {
  const other = 'o'.repeat(32)
  beforeEach(() => {
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'alpha' }), [other]: nodeEntry({ id: other, name: 'beta' }) }, order: [NODE, other], loaded: true })
  })

  it('shows a labelled dot on the node filter button when nodes outside the filter have things waiting, while the nav count stays on the ticked nodes', () => {
    useSessions.getState().addPending(NODE, { request_id: 'r1', session_id: 'x', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    useSessions.getState().addPending(other, { request_id: 'r2', session_id: 'y', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    useSessions.getState().addPending(other, { request_id: 'r3', session_id: 'z', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    usePrefs.setState({ sidebarNodes: [NODE] })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-inbox-count')).toHaveTextContent('1')
    expect(screen.getByTestId('node-filter-dot')).toHaveAccessibleName('2 waiting on nodes outside the filter')
  })

  it('has no dot when everything is ticked or nothing waits elsewhere', () => {
    useSessions.getState().addPending(other, { request_id: 'r2', session_id: 'y', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('node-filter-dot')).toBeNull()
  })
})


describe('to-do entries need a node that supports to-dos', () => {
  it('without one the sidebar has no To-dos item and no +; with one it has both', () => {
    const { unmount } = wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-todos')).toBeNull()
    expect(screen.queryByTestId('sidebar-capture')).toBeNull()
    unmount()
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'alpha', info: { capabilities: { todo: true } } as never, agents: [mockAgent()], projects: [project({ name: 'app' })] }) }, order: [NODE], loaded: true })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-todos')).toBeInTheDocument()
    expect(screen.getByTestId('sidebar-capture')).toBeInTheDocument()
  })
})
