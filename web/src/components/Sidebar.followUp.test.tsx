// 左栏里的「待处理」（web.md § 4.6.1）：停下来的待处理会话置顶成一组（等得最久的在最上面），
// 还在跑的留在 Recent 里带一个空心旗，未读的亮小红点；窄条上显示个数。
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

describe('sidebar: follow-up group', () => {
  it('lists stopped follow-up sessions, longest-waiting first, and leaves out running, archived and unmarked ones', () => {
    seed(
      row({ id: 'new', title: 'Finished recently', follow_up: true, status: 'idle', updated_at_ms: 2_000 }),
      row({ id: 'old', title: 'Finished long ago', follow_up: true, status: 'idle', updated_at_ms: 1_000 }),
      row({ id: 'run', title: 'Still running', follow_up: true, status: 'running', updated_at_ms: 3_000 }),
      row({ id: 'arch', title: 'Archived one', follow_up: true, archived: true, updated_at_ms: 4_000 }),
      row({ id: 'plain', title: 'Plain', updated_at_ms: 5_000 }),
    )
    wrap(<Sidebar nodeId={NODE} />)
    const group = screen.getByTestId('sidebar-follow-up')
    expect(within(group).getAllByRole('link').map((a) => a.textContent)).toEqual([expect.stringContaining('Finished long ago'), expect.stringContaining('Finished recently')])
    expect(within(group).queryByText('Still running')).toBeNull()
  })

  it('has no group when nothing is waiting, and a session that needs approval stays in the approval group only', () => {
    seed(row({ id: 'p', title: 'Plain' }))
    const { unmount } = wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-follow-up')).toBeNull()
    unmount()
    seed(row({ id: 'w', title: 'Approve me', follow_up: true, status: 'waiting_permission', pending_permissions: 1 }))
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-follow-up')).toBeNull()
    expect(within(screen.getByTestId('sidebar-waiting')).getByText('Approve me')).toBeInTheDocument()
  })

  it('a running follow-up session keeps its place in Recent, with a hollow flag', () => {
    seed(row({ id: 'run', title: 'Still running', follow_up: true, status: 'running' }))
    wrap(<Sidebar nodeId={NODE} />)
    const flag = within(screen.getByTestId('sidebar-recent')).getByTestId('sidebar-follow-up-flag')
    expect(flag.getAttribute('class')).not.toMatch(/fill-current/)
  })

  it('shows the unread dot on a stopped session with unseen output, but not on the open one', () => {
    seed(row({ id: 'u', title: 'Unseen', status: 'idle', last_seq: 8, read_seq: 2 }), row({ id: 'r', title: 'Seen', status: 'idle', last_seq: 8, read_seq: 8 }))
    wrap(<Sidebar nodeId={NODE} />)
    const unseen = screen.getAllByText('Unseen')[0]!.closest('a')!
    expect(within(unseen).getByTestId('unread-dot')).toBeInTheDocument()
    expect(within(screen.getAllByText('Seen')[0]!.closest('a')!).queryByTestId('unread-dot')).toBeNull()
  })

  it('the narrow rail shows how many are waiting for follow-up', () => {
    seed(row({ id: 'a', follow_up: true, status: 'idle' }), row({ id: 'b', follow_up: true, status: 'error' }))
    wrap(<SidebarRail nodeId={NODE} onSearch={() => {}} onPeek={() => {}} onPin={() => {}} />)
    expect(screen.getByTestId('sidebar-rail-follow-up')).toHaveTextContent('2')
  })
})
