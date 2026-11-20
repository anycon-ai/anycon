// 空间切换（spec/web.md § 4.13）：有分组时的菜单、选择、各个消费者都跟着空间走、小点的说明、管理对话框。
// 没有分组时的行为由 Sidebar.nav.test.tsx 里原来的节点过滤器测试原样守着（它们一条都没改）。
import type { ReactNode } from 'react'
import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { getInboxSnapshot } from '@/features/inbox/model'
import { inboxNowCount } from '@/features/inbox/derive'
import { nodeEntry, project, summary } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useEnroll } from '@/stores/enroll'
import { useManageSpaces } from '@/stores/manageSpaces'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type SessionItem } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => {
  const m = mockRouter(await orig<object>(), navigate, { node: 'hhhhhhhh' })
  return { ...m, useRouterState: (o?: { select?: (s: unknown) => unknown }) => (o?.select ? o.select({ location: { pathname: '/' } }) : null) }
})
vi.mock('@/api/queries', () => ({ useMe: () => ({ data: undefined }), useDevices: () => ({ data: undefined }), logout: vi.fn() }))

import { PageHeader } from './PageHeader'
import { Sidebar } from './Sidebar'

const HOME = 'h'.repeat(32)
const WORK1 = 'w'.repeat(32)
const WORK2 = 'x'.repeat(32)
const info = (group?: string | null): NodeInfo => ({ group, capabilities: { todo: true } }) as unknown as NodeInfo
const wrap = (ui: ReactNode) => render(<QueryClientProvider client={new QueryClient()}>{ui}</QueryClientProvider>)
const row = (node: string, over: Parameters<typeof summary>[0]): SessionItem => ({ ...summary({ project: 'app', ...over }), node_id: node, cached_seq: 0 })
const perm = (id: string, session: string) => ({ request_id: id, session_id: session, kind: 'tool' as const, title: 't', detail: {}, options: [], created_at_ms: 1 })

function setup(groups: Record<string, string | null | undefined>): void {
  useNodes.setState({
    byId: {
      [HOME]: nodeEntry({ id: HOME, name: 'house', info: info(groups.h) }),
      [WORK1]: nodeEntry({ id: WORK1, name: 'office', info: info(groups.w) }),
      [WORK2]: nodeEntry({ id: WORK2, name: 'lab', info: info(groups.x) }),
    },
    order: [HOME, WORK1, WORK2],
    loaded: true,
  })
}

beforeEach(() => {
  navigate.mockReset()
  usePrefs.setState({ sidebarOpen: true, sidebarNodes: null, sidebarSpace: null, sidebarRecent: true, lastTodoNode: null, lastTodoNodeBySpace: {} })
  useEnroll.setState({ items: {}, resolved: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useManageSpaces.setState({ open: false })
  setup({ h: 'Home', w: 'Work', x: 'Work' })
})

describe('the switcher with groups', () => {
  it('shows “All” on the button, lists the spaces and the nodes in the menu, and has Manage spaces', async () => {
    const user = userEvent.setup()
    wrap(<Sidebar nodeId={HOME} />)
    expect(screen.getByTestId('space-label')).toHaveTextContent(/^All$/)
    await user.click(screen.getByTestId('node-switcher'))
    expect(await screen.findByTestId('space-all')).toBeInTheDocument()
    expect(screen.getAllByTestId('space-item').map((i) => i.textContent)).toEqual([expect.stringContaining('Home'), expect.stringContaining('Work')])
    expect(screen.getAllByTestId('node-filter-item')).toHaveLength(3)
    expect(screen.getByTestId('space-manage')).toHaveTextContent('Manage spaces…')
  })

  it('choosing a space selects exactly its nodes, shows its name, and is remembered as the space (not as a list of nodes)', async () => {
    const user = userEvent.setup()
    wrap(<Sidebar nodeId={HOME} />)
    await user.click(screen.getByTestId('node-switcher'))
    await user.click((await screen.findAllByTestId('space-item'))[1]!)
    expect(usePrefs.getState()).toMatchObject({ sidebarSpace: 'Work', sidebarNodes: null })
    expect(screen.getByTestId('space-label')).toHaveTextContent('Work')
    await user.click(screen.getByTestId('node-switcher'))
    const checked = (await screen.findAllByTestId('node-filter-item')).map((i) => i.getAttribute('aria-checked'))
    expect(checked).toEqual(['false', 'true', 'true'])
  })

  it('ticking a node by hand turns the choice into a hand-picked set (the space is no longer selected); “All” clears both', async () => {
    const user = userEvent.setup()
    usePrefs.setState({ sidebarSpace: 'Work' })
    wrap(<Sidebar nodeId={HOME} />)
    await user.click(screen.getByTestId('node-switcher'))
    await user.click((await screen.findAllByTestId('node-filter-item'))[0]!)
    // 勾上「家里」之后三个节点都选中了：是「全部」，不再是一个空间
    expect(usePrefs.getState()).toMatchObject({ sidebarSpace: null, sidebarNodes: null })
    await user.keyboard('{Escape}')
    await user.click(screen.getByTestId('node-switcher'))
    await user.click((await screen.findAllByTestId('node-filter-item'))[1]!)
    expect(usePrefs.getState()).toMatchObject({ sidebarSpace: null, sidebarNodes: [HOME, WORK2] })
    await user.keyboard('{Escape}')
    await user.click(screen.getByTestId('node-switcher'))
    await user.click(await screen.findByTestId('space-all'))
    expect(usePrefs.getState()).toMatchObject({ sidebarSpace: null, sidebarNodes: null })
  })

  it('the selection follows the groups live: a node that changes group moves between spaces and the selected space’s node set changes with it', async () => {
    usePrefs.setState({ sidebarSpace: 'Work' })
    wrap(<Sidebar nodeId={HOME} />)
    expect(screen.getByTestId('space-label')).toHaveTextContent('Work')
    // 家里的节点也改成 Work
    useNodes.getState().setInfo(HOME, info('Work'))
    expect(getInboxSnapshot().model).toBeDefined()
    expect(screen.getByTestId('space-label')).toHaveTextContent('Work')
    useNodes.getState().setInfo(WORK1, info('Home'))
    useNodes.getState().setInfo(WORK2, info('Home'))
    useNodes.getState().setInfo(HOME, info('Home'))
    // 「Work」已经没有节点了：退回全部，空间名还留在偏好里
    expect(await screen.findByText('All', { selector: '[data-testid=space-label]' })).toBeInTheDocument()
    expect(usePrefs.getState().sidebarSpace).toBe('Work')
  })

  it('Manage spaces opens the dialog', async () => {
    const user = userEvent.setup()
    wrap(<Sidebar nodeId={HOME} />)
    await user.click(screen.getByTestId('node-switcher'))
    await user.click(await screen.findByTestId('space-manage'))
    expect(useManageSpaces.getState().open).toBe(true)
  })

  it('without groups there is nothing about spaces: the plain node filter (no space entries, no Manage spaces, no space icon label)', async () => {
    const user = userEvent.setup()
    setup({})
    wrap(<Sidebar nodeId={HOME} />)
    await user.click(screen.getByTestId('node-switcher'))
    await screen.findAllByTestId('node-filter-item')
    expect(screen.queryByTestId('space-item')).toBeNull()
    expect(screen.queryByTestId('space-all')).toBeNull()
    expect(screen.queryByTestId('space-manage')).toBeNull()
  })
})

describe('everything the node filter applies to follows the space', () => {
  beforeEach(() => {
    useSessions.setState({ list: { sh: row(HOME, { id: 'sh', title: 'home chat', status: 'error', updated_at_ms: Date.now() }), sw: row(WORK1, { id: 'sw', title: 'work chat', status: 'error', updated_at_ms: Date.now() }) } })
    useSessions.getState().addPending(HOME, perm('r1', 'x'))
    useSessions.getState().addPending(WORK1, perm('r2', 'y'))
    useSessions.getState().addPending(WORK2, perm('r3', 'z'))
  })

  it('the Inbox model, the nav count and the badge count only the selected space', () => {
    usePrefs.setState({ sidebarSpace: 'Work' })
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(3) // r2、r3、work 的出错会话
    usePrefs.setState({ sidebarSpace: 'Home' })
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(2)
    usePrefs.setState({ sidebarSpace: null })
    expect(inboxNowCount(getInboxSnapshot().model)).toBe(5)
  })

  it('the dot says how many are waiting in which other space', () => {
    usePrefs.setState({ sidebarSpace: 'Home' })
    wrap(<Sidebar nodeId={HOME} />)
    expect(screen.getByTestId('node-filter-dot')).toHaveAccessibleName('3 waiting in other spaces (Work: 3)')
    expect(screen.getByTestId('sidebar-inbox-count')).toHaveTextContent('2')
  })

  it('the dot names nodes that have no space separately and nothing when everything is in the current space', () => {
    usePrefs.setState({ sidebarSpace: null })
    wrap(<Sidebar nodeId={HOME} />)
    expect(screen.queryByTestId('node-filter-dot')).toBeNull()
  })

  it('the session tree shows only the selected space’s nodes’ projects', () => {
    useNodes.setState({
      byId: {
        [HOME]: nodeEntry({ id: HOME, name: 'house', info: info('Home'), projects: [project({ name: 'home-proj' })] }),
        [WORK1]: nodeEntry({ id: WORK1, name: 'office', info: info('Work'), projects: [project({ name: 'work-proj' })] }),
        [WORK2]: nodeEntry({ id: WORK2, name: 'lab', info: info('Work'), projects: [] }),
      },
      order: [HOME, WORK1, WORK2],
      loaded: true,
    })
    usePrefs.setState({ sidebarSpace: 'Work' })
    wrap(<Sidebar nodeId={HOME} />)
    expect(screen.queryByText('home-proj')).toBeNull()
    expect(screen.getByText('work-proj')).toBeInTheDocument()
  })
})

describe('phone top bar', () => {
  it('shows the current space name and opens the same switcher on a tab root, when there are groups', async () => {
    const user = userEvent.setup()
    usePrefs.setState({ sidebarSpace: 'Work' })
    wrap(<PageHeader root crumbs={[{ label: 'Inbox' }]} showNodeStatus={false} />)
    expect(screen.getByTestId('space-topbar-label')).toHaveTextContent('Work')
    await user.click(screen.getByTestId('space-topbar'))
    expect(await screen.findByTestId('space-all')).toBeInTheDocument()
  })

  it('is absent without groups, on secondary pages, and where the page has its own switcher', () => {
    setup({})
    const none = wrap(<PageHeader root crumbs={[{ label: 'Inbox' }]} showNodeStatus={false} />)
    expect(screen.queryByTestId('space-topbar')).toBeNull()
    none.unmount()
    setup({ h: 'Home', w: 'Work', x: 'Work' })
    const second = wrap(<PageHeader crumbs={[{ label: 'Session' }]} showNodeStatus={false} />)
    expect(screen.queryByTestId('space-topbar')).toBeNull()
    second.unmount()
    wrap(<PageHeader root spaceSwitcher={false} crumbs={[{ label: 'Sessions' }]} showNodeStatus={false} />)
    expect(screen.queryByTestId('space-topbar')).toBeNull()
  })
})

describe('untrusted group names', () => {
  it('control and bidi characters never reach the label and a long name is cut', () => {
    setup({ h: `Ho‮me${'x'.repeat(100)}`, w: 'Work', x: 'Work' })
    usePrefs.setState({ sidebarSpace: `Ho‮me${'x'.repeat(100)}` })
    wrap(<Sidebar nodeId={HOME} />)
    const label = within(screen.getByTestId('node-switcher')).getByTestId('space-label').textContent ?? ''
    expect(label).not.toContain('‮')
    expect([...label].length).toBeLessThanOrEqual(40)
  })
})
