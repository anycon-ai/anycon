// 左栏：项目行右边的「＋」在该项目里新建会话（左栏没有单独的「新会话」按钮）；顶部的节点下拉是多选过滤；宽屏左栏可以收起（偏好记在 localStorage，⌘/Ctrl+B 开合）。
import type { ReactNode } from 'react'
import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mockAgent, nodeEntry, project, resetSeq, summary } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useEnroll } from '@/stores/enroll'
import { useNewProject } from '@/stores/newProject'

const navigate = vi.hoisted(() => vi.fn())
const devices = vi.hoisted(() => ({ data: undefined as unknown, dataUpdatedAt: 0 }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))
vi.mock('@/api/queries', () => ({ useMe: () => ({ data: undefined }), useDevices: () => devices, logout: vi.fn() }))

import { PageHeader } from './PageHeader'
import { Sidebar } from './Sidebar'
import { useShortcuts } from './useShortcuts'

const NODE = 'n'.repeat(32)

function wrap(ui: ReactNode) {
  return render(<QueryClientProvider client={new QueryClient()}>{ui}</QueryClientProvider>)
}

function Shortcuts() {
  useShortcuts()
  return null
}

beforeEach(() => {
  resetSeq()
  navigate.mockReset()
  devices.data = undefined
  devices.dataUpdatedAt = 0
  useNewProject.setState({ request: null })
  usePrefs.setState({ sidebarOpen: true, sidebarNodes: null })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects: [project({ name: 'app' }), project({ name: 'web' })] }) }, order: [NODE], loaded: true })
  useSessions.setState({
    list: {
      a: { ...summary({ id: 'a', title: 'In app', project: 'app' }), node_id: NODE, cached_seq: 0 },
      b: { ...summary({ id: 'b', title: 'In web', project: 'web' }), node_id: NODE, cached_seq: 0 },
    },
    pending: {},
    open: {},
  })
})

describe('sidebar project rows', () => {
  it('each project has a “+” that opens a blank new session in that project', () => {
    wrap(<Sidebar nodeId={NODE} />)
    const plus = screen.getAllByTestId('sidebar-project-new')
    expect(plus.map((a) => a.getAttribute('data-project')).sort()).toEqual(['app', 'web'])
    const app = plus.find((a) => a.getAttribute('data-project') === 'app')!
    expect(app).toHaveAttribute('href', '/n/nnnnnnnn/new?project=app')
    expect(app).toHaveAccessibleName('New session in app')
  })

  it('has no standalone “New session” button, and the sessions under a project have no guide line', () => {
    wrap(<Sidebar nodeId={NODE} onSearch={() => {}} />)
    expect(screen.queryByTestId('sidebar-new-session')).toBeNull()
    expect(screen.queryByRole('button', { name: 'New session' })).toBeNull()
    expect(screen.getByText('In app').closest('a')!.parentElement!.className).not.toMatch(/\bborder-l\b/)
    expect(screen.getByRole('button', { name: /Search/ })).toBeInTheDocument()
  })

  it('the collapse button only exists where the caller can collapse (not in the phone drawer)', () => {
    const { unmount } = wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-collapse')).toBeNull()
    unmount()
    const onCollapse = vi.fn()
    wrap(<Sidebar nodeId={NODE} onCollapse={onCollapse} />)
    screen.getByTestId('sidebar-collapse').click()
    expect(onCollapse).toHaveBeenCalledTimes(1)
  })
})

describe('collapsing the wide-screen sidebar', () => {
  it('the page header offers “show sidebar” only while it is collapsed', async () => {
    const user = userEvent.setup()
    usePrefs.setState({ sidebarOpen: false })
    wrap(<PageHeader crumbs={[{ label: 'x' }]} />)
    await user.click(screen.getByTestId('sidebar-expand'))
    expect(usePrefs.getState().sidebarOpen).toBe(true)
    expect(screen.queryByTestId('sidebar-expand')).toBeNull()
  })

  it('⌘/Ctrl+B toggles it, and holding the keys down does not flicker', async () => {
    const user = userEvent.setup()
    wrap(<Shortcuts />)
    await user.keyboard('{Control>}b{/Control}')
    expect(usePrefs.getState().sidebarOpen).toBe(false)
    await user.keyboard('{Meta>}b{/Meta}')
    expect(usePrefs.getState().sidebarOpen).toBe(true)
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'b', ctrlKey: true, repeat: true }))
    expect(usePrefs.getState().sidebarOpen).toBe(true)
  })
})

describe('empty sidebar', () => {
  const NODE_B = 'b'.repeat(32)

  it('offers “New project”, which opens the new-project dialog right here and closes the drawer', async () => {
    const user = userEvent.setup()
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, projects: [] }) }, order: [NODE], loaded: true })
    useSessions.setState({ list: {}, pending: {}, open: {} })
    const onNavigate = vi.fn()
    wrap(<Sidebar nodeId={NODE} onNavigate={onNavigate} />)
    const button = screen.getByTestId('sidebar-new-project')
    expect(button).toHaveTextContent('New project')
    await user.click(button)
    expect(onNavigate).toHaveBeenCalled()
    expect(navigate).not.toHaveBeenCalled()
    expect(useNewProject.getState().request).toEqual({ nodeId: NODE, mode: 'empty', then: 'project', pickNode: true })
  })

  it('starts on the node the page is on when several nodes are ticked, and is absent once there is anything to show', async () => {
    const user = userEvent.setup()
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE }), [NODE_B]: nodeEntry({ id: NODE_B }) }, order: [NODE, NODE_B], loaded: true })
    useSessions.setState({ list: {}, pending: {}, open: {} })
    const { unmount } = wrap(<Sidebar nodeId={NODE_B} />)
    await user.click(screen.getByTestId('sidebar-new-project'))
    expect(useNewProject.getState().request?.nodeId).toBe(NODE_B)
    unmount()
    // 页面所在的节点没被勾选：用第一个勾选的
    usePrefs.setState({ sidebarNodes: [NODE] })
    const second = wrap(<Sidebar nodeId={NODE_B} />)
    await user.click(screen.getByTestId('sidebar-new-project'))
    expect(useNewProject.getState().request?.nodeId).toBe(NODE)
    second.unmount()
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, projects: [project({ name: 'app' })] }) }, order: [NODE], loaded: true })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-empty')).toBeNull()
    expect(screen.queryByTestId('sidebar-new-project')).toBeNull()
  })
})

describe('“New project” next to search', () => {
  it('is always there while a node is ticked, and opens the dialog with a node picker', async () => {
    const user = userEvent.setup()
    wrap(<Sidebar nodeId={NODE} onSearch={() => {}} />)
    expect(screen.getAllByTestId('sidebar-project-new').length).toBeGreaterThan(0) // 已经有项目了，按钮照样在
    const button = screen.getByTestId('sidebar-new-project-button')
    expect(button).toHaveAccessibleName('New project')
    await user.click(button)
    expect(useNewProject.getState().request).toEqual({ nodeId: NODE, mode: 'empty', then: 'project', pickNode: true })
  })

  it('is absent when this browser has no node', () => {
    useNodes.setState({ byId: {}, order: [], loaded: true })
    wrap(<Sidebar nodeId={undefined} />)
    expect(screen.queryByTestId('sidebar-new-project-button')).toBeNull()
  })
})

describe('node filter — which nodes’ projects the sidebar shows', () => {
  const NODE_B = 'b'.repeat(32)
  beforeEach(() => {
    useNodes.setState({
      byId: {
        [NODE]: nodeEntry({ id: NODE, name: 'laptop', projects: [project({ name: 'app' })] }),
        [NODE_B]: nodeEntry({ id: NODE_B, name: 'nas', projects: [project({ name: 'app' }), project({ name: 'backup' })] }),
      },
      order: [NODE, NODE_B],
      loaded: true,
    })
    useSessions.setState({
      list: {
        a: { ...summary({ id: 'a', title: 'On laptop', project: 'app' }), node_id: NODE, cached_seq: 0 },
        b: { ...summary({ id: 'b', title: 'On nas', project: 'app' }), node_id: NODE_B, cached_seq: 0 },
        c: { ...summary({ id: 'c', title: 'Scratch on nas' }), node_id: NODE_B, cached_seq: 0 },
      },
      pending: {},
      open: {},
    })
  })

  const projectRows = (): string[] => screen.getAllByTestId('sidebar-project-new').map((a) => `${a.getAttribute('href')}`)

  it('shows the projects of every node by default, same-named projects apart and labelled with their node', () => {
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('node-switcher')).toHaveTextContent('All nodes')
    expect(projectRows().sort()).toEqual(['/n/bbbbbbbb/new?project=app', '/n/bbbbbbbb/new?project=backup', '/n/nnnnnnnn/new?project=app'])
    expect(screen.getByText('On laptop')).toBeInTheDocument()
    expect(screen.getByText('On nas')).toBeInTheDocument()
    expect(screen.getByText('Scratch on nas').closest('a')).toHaveAttribute('href', '/n/bbbbbbbb/s/c')
    // 勾了多个节点，项目行和临时会话行上标节点名
    expect(screen.getAllByText('laptop')).toHaveLength(1) // app
    expect(screen.getAllByText('nas')).toHaveLength(3) // app、backup、临时会话
  })

  it('ticking nodes in the dropdown filters the tree and keeps the menu open for the next tick', async () => {
    const user = userEvent.setup()
    wrap(<Sidebar nodeId={NODE} />)
    await user.click(screen.getByTestId('node-switcher'))
    const [laptop, nas] = await screen.findAllByTestId('node-filter-item')
    expect(laptop).toHaveAttribute('aria-checked', 'true')
    expect(nas).toHaveAttribute('aria-checked', 'true')

    await user.click(nas!)
    expect(usePrefs.getState().sidebarNodes).toEqual([NODE])
    expect(screen.getAllByTestId('node-filter-item')).toHaveLength(2) // 菜单没关
    expect(nas).toHaveAttribute('aria-checked', 'false')
    expect(projectRows()).toEqual(['/n/nnnnnnnn/new?project=app'])
    expect(screen.queryByText('On nas')).toBeNull()
    expect(screen.queryByText('Scratch on nas')).toBeNull()
    expect(screen.getByTestId('node-switcher')).toHaveTextContent('laptop') // 只剩一个节点：显示它的名字

    await user.click(nas!)
    expect(usePrefs.getState().sidebarNodes).toBeNull() // 全勾回去就是「全部」，之后配对的新节点也会显示
    expect(projectRows()).toHaveLength(3)
  })

  it('never lets the last ticked node be unticked', async () => {
    const user = userEvent.setup()
    usePrefs.setState({ sidebarNodes: [NODE_B] })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('node-switcher')).toHaveTextContent('nas')
    await user.click(screen.getByTestId('node-switcher'))
    const [, nas] = await screen.findAllByTestId('node-filter-item')
    await user.click(nas!)
    expect(usePrefs.getState().sidebarNodes).toEqual([NODE_B])
    expect(nas).toHaveAttribute('aria-checked', 'true')
    expect(projectRows().sort()).toEqual(['/n/bbbbbbbb/new?project=app', '/n/bbbbbbbb/new?project=backup'])
  })

  it('goes back to all nodes when the saved choice names nodes that no longer exist', () => {
    usePrefs.setState({ sidebarNodes: ['x'.repeat(32)] })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('node-switcher')).toHaveTextContent('All nodes')
    expect(projectRows()).toHaveLength(3)
  })

  it('shows “n of m nodes” when some but not all of three are ticked', () => {
    const NODE_C = 'c'.repeat(32)
    useNodes.setState({ byId: { ...useNodes.getState().byId, [NODE_C]: nodeEntry({ id: NODE_C, name: 'pi' }) }, order: [NODE, NODE_B, NODE_C] })
    usePrefs.setState({ sidebarNodes: [NODE, NODE_B] })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('node-switcher')).toHaveTextContent('2 of 3 nodes')
  })
})

describe('node switcher — nodes of the account this browser has not unlocked (ADR-36)', () => {
  const daemon = (id: string, name: string) => ({ id, name, kind: 'daemon', platform: 'linux', created_at_ms: 1, online: true, paired_with: [] })

  it('lists them at the bottom of the dropdown with a lock, and a click goes to /unlock/:node', async () => {
    const user = userEvent.setup()
    devices.data = [daemon(NODE, 'paired-one'), daemon('b'.repeat(32), 'nas'), { ...daemon('w'.repeat(32), 'a browser'), kind: 'web' }]
    wrap(<Sidebar nodeId={NODE} />)
    await user.click(screen.getByTestId('node-switcher'))
    const locked = await screen.findAllByTestId('node-locked')
    expect(locked).toHaveLength(1) // 配对过的和不是节点的设备不在里面
    expect(locked[0]).toHaveTextContent('nas')
    expect(locked[0]!.querySelector('svg')).not.toBeNull() // 锁图标
    // 排在已配对的节点后面、「节点设置」「配对另一台」前面
    const all = [...screen.getByRole('menu').querySelectorAll('[role^="menuitem"]')].map((e) => e.textContent ?? '')
    expect(all.findIndex((t) => t.includes('nas'))).toBeGreaterThan(all.findIndex((t) => t.includes('test-node')))
    expect(all.findIndex((t) => t.includes('nas'))).toBeLessThan(all.findIndex((t) => /Node settings|节点设置/.test(t)))
    await user.click(locked[0]!)
    expect(navigate).toHaveBeenCalledWith({ to: '/unlock/$node', params: { node: 'bbbbbbbb' } })
  })

  it('has no such section when everything on the account is unlocked', async () => {
    const user = userEvent.setup()
    devices.data = [daemon(NODE, 'paired-one')]
    wrap(<Sidebar nodeId={NODE} />)
    await user.click(screen.getByTestId('node-switcher'))
    await screen.findByTestId('node-add')
    expect(screen.queryByTestId('node-locked')).toBeNull()
  })
})

describe('node switcher — nodes removed from the account on another device', () => {
  const daemon = (id: string, name: string) => ({ id, name, kind: 'daemon', platform: 'linux', created_at_ms: 1, online: true, paired_with: [] })
  const NODE_B = 'b'.repeat(32)
  beforeEach(() => {
    useNodes.setState({
      byId: { [NODE]: nodeEntry({ id: NODE, name: 'laptop', pairedAt: 100 }), [NODE_B]: nodeEntry({ id: NODE_B, name: 'old-mini', pairedAt: 100 }) },
      order: [NODE, NODE_B],
      loaded: true,
    })
  })

  it('lists a paired node the account no longer has; a click goes to its page (where it can be forgotten)', async () => {
    const user = userEvent.setup()
    devices.data = [daemon(NODE, 'laptop')]
    devices.dataUpdatedAt = 200
    wrap(<Sidebar nodeId={NODE} />)
    await user.click(screen.getByTestId('node-switcher'))
    const removed = await screen.findAllByTestId('node-removed')
    expect(removed).toHaveLength(1)
    expect(removed[0]).toHaveTextContent('old-mini')
    await user.click(removed[0]!)
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node', params: { node: 'bbbbbbbb' } })
  })

  it('does not count a device list older than the pairing (a node paired just now may not be in it yet)', async () => {
    const user = userEvent.setup()
    devices.data = [daemon(NODE, 'laptop')]
    devices.dataUpdatedAt = 50
    wrap(<Sidebar nodeId={NODE} />)
    await user.click(screen.getByTestId('node-switcher'))
    await screen.findByTestId('node-add')
    expect(screen.queryByTestId('node-removed')).toBeNull()
  })
})

describe('account menu — scan to approve (ADR-36)', () => {
  it('is offered only when the device has a camera, and opens the scanner', async () => {
    const user = userEvent.setup()
    const { unmount } = wrap(<Sidebar nodeId={NODE} />)
    await user.click(screen.getByTestId('account-menu'))
    await screen.findByTestId('logout')
    expect(screen.queryByTestId('scan-approve')).toBeNull()
    unmount()

    Object.defineProperty(navigator, 'mediaDevices', { configurable: true, value: { getUserMedia: () => {}, enumerateDevices: async () => [{ kind: 'videoinput' }] } })
    try {
      wrap(<Sidebar nodeId={NODE} />)
      await screen.findByTestId('account-menu')
      await act(async () => {
        await new Promise((r) => setTimeout(r, 0)) // 让「有没有摄像头」的异步检查跑完
      })
      await user.click(screen.getByTestId('account-menu'))
      await user.click(await screen.findByTestId('scan-approve'))
      expect(useEnroll.getState().scannerOpen).toBe(true)
    } finally {
      delete (navigator as { mediaDevices?: unknown }).mediaDevices
      useEnroll.setState({ scannerOpen: false })
    }
  })
})

describe('account menu — notification settings (ADR-37)', () => {
  it('has a "Notification settings" entry that opens /settings/notifications', async () => {
    const user = userEvent.setup()
    wrap(<Sidebar nodeId={NODE} />)
    await user.click(screen.getByTestId('account-menu'))
    await user.click(await screen.findByTestId('notification-settings'))
    expect(navigate).toHaveBeenCalledWith({ to: '/settings/notifications' })
  })
})
