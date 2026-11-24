// 左栏里的「完成后提醒」标注（web.md § 4.6.1）：左栏没有置顶组；标了的会话（任何状态）带同一种旗，未读的亮小红点；
// 窄条上只有「待处理」的一个「现在」个数（标记不计入）。
import type { ReactNode } from 'react'
import { render, screen, within } from '@testing-library/react'
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

import { Sidebar, SidebarRail } from './Sidebar'

const NODE = 'n'.repeat(32)
const wrap = (ui: ReactNode) => render(<QueryClientProvider client={new QueryClient()}>{ui}</QueryClientProvider>)
const row = (over: Parameters<typeof summary>[0]): SessionItem => ({ ...summary({ project: 'app', ...over }), node_id: NODE, cached_seq: 0 })
const seed = (...items: SessionItem[]): void => useSessions.setState({ list: Object.fromEntries(items.map((i) => [i.id, i])), pending: {}, open: {} })

beforeEach(() => {
  resetSeq()
  usePrefs.setState({ sidebarOpen: true, sidebarNodes: null, sidebarRecent: true })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects: [project({ name: 'app' })] }) }, order: [NODE], loaded: true })
})

describe('sidebar: follow-up marks', () => {
  it('has no pinned groups: marked sessions and approvals are not pulled out of the project tree', () => {
    seed(
      row({ id: 'new', title: 'Finished recently', follow_up: true, status: 'idle', updated_at_ms: 2_000 }),
      row({ id: 'ask', title: 'Approve me', follow_up: true, status: 'waiting_permission', pending_permissions: 1 }),
    )
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-follow-up')).toBeNull()
    expect(screen.queryByTestId('sidebar-waiting')).toBeNull()
    // 项目树里它们照常在
    expect(screen.getByText('Finished recently')).toBeInTheDocument()
    expect(screen.getByText('Approve me')).toBeInTheDocument()
  })

  it('a marked session shows the same flag whether it stopped or still runs', () => {
    seed(row({ id: 'new', title: 'Finished recently', follow_up: true, status: 'idle' }), row({ id: 'run', title: 'Still running', follow_up: true, status: 'running' }))
    wrap(<Sidebar nodeId={NODE} />)
    const idle = within(screen.getByText('Finished recently').closest('a')!).getByTestId('sidebar-follow-up-flag')
    const running = within(screen.getByTestId('sidebar-recent')).getByTestId('sidebar-follow-up-flag')
    expect(idle).toHaveAccessibleName('Notify when done')
    expect(idle.getAttribute('class')).not.toMatch(/fill-current/)
    expect(running.getAttribute('class')).toBe(idle.getAttribute('class'))
  })

  it('shows the unread dot on a stopped session with unseen output, but not on the open one', () => {
    seed(row({ id: 'u', title: 'Unseen', status: 'idle', last_seq: 8, read_seq: 2 }), row({ id: 'r', title: 'Seen', status: 'idle', last_seq: 8, read_seq: 8 }))
    wrap(<Sidebar nodeId={NODE} />)
    const unseen = screen.getAllByText('Unseen')[0]!.closest('a')!
    expect(within(unseen).getByTestId('unread-dot')).toBeInTheDocument()
    expect(within(screen.getAllByText('Seen')[0]!.closest('a')!).queryByTestId('unread-dot')).toBeNull()
  })

  it('the narrow rail shows one count, the inbox “Now” count: approvals and errors together (a follow-up mark is not counted), and not two chips', () => {
    seed(row({ id: 'a', follow_up: true, status: 'idle' }), row({ id: 'b', status: 'error', updated_at_ms: Date.now() }), row({ id: 'c', pending_permissions: 1, status: 'waiting_permission' }))
    useSessions.getState().addPending(NODE, { request_id: 'r1', session_id: 'c', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    wrap(<SidebarRail nodeId={NODE} onSearch={() => {}} onPin={() => {}} />)
    const link = screen.getByTestId('sidebar-rail-inbox')
    expect(link).toHaveAttribute('href', '/inbox')
    expect(link).toHaveAccessibleName('Inbox, 2 waiting')
    expect(screen.getByTestId('sidebar-rail-inbox-count')).toHaveTextContent('2')
    expect(screen.queryByTestId('sidebar-rail-waiting')).toBeNull()
    expect(screen.queryByTestId('sidebar-rail-follow-up')).toBeNull()
    expect(screen.getAllByText('2')).toHaveLength(1)
  })

  it('the rail has the inbox link without a number when nothing waits', () => {
    seed(row({ id: 'p' }))
    wrap(<SidebarRail nodeId={NODE} onSearch={() => {}} onPin={() => {}} />)
    expect(screen.getByTestId('sidebar-rail-inbox')).toHaveAccessibleName('Inbox')
    expect(screen.queryByTestId('sidebar-rail-inbox-count')).toBeNull()
  })
})
