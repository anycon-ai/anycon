// 左栏：项目行右边的「＋」在该项目里新建会话；宽屏左栏可以收起（偏好记在 localStorage，⌘/Ctrl+B 开合）。
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

const navigate = vi.hoisted(() => vi.fn())
const devices = vi.hoisted(() => ({ data: undefined as unknown }))
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
  usePrefs.setState({ sidebarOpen: true })
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
    const all = screen.getAllByRole('menuitem').map((e) => e.textContent ?? '')
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
