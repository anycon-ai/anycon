// 外壳在三种宽度下的导航（spec/web.md § 3）：手机上左栏是抽屉，顶栏的菜单按钮打开、点了去处就收起；宽屏左栏常驻。
import type { ReactNode } from 'react'
import { act, render as rtlRender, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { resetViewport, setViewport } from '@/test/viewport'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const route = vi.hoisted(() => ({ pathname: '/inbox' }))
const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => {
  const m = mockRouter(await orig<object>(), navigate, {})
  return {
    ...m,
    Outlet: () => <div data-testid="outlet" />,
    useRouterState: (o?: { select?: (s: unknown) => unknown }) => (o?.select ? o.select({ location: { pathname: route.pathname } }) : null),
  }
})
vi.mock('@/api/queries', () => ({ useMe: () => ({ data: undefined }), useDevices: () => ({ data: undefined }), logout: vi.fn() }))
vi.mock('./CommandPalette', () => ({ CommandPalette: () => null }))
vi.mock('./NewProjectHost', () => ({ NewProjectHost: () => null }))
vi.mock('./StatusBanner', () => ({ StatusBanner: () => null }))
vi.mock('@/features/approve/EnrollApprovalDialog', () => ({ EnrollApprovalDialog: () => null }))
vi.mock('@/features/approve/ScanApproveDialog', () => ({ ScanApproveDialog: () => null }))
vi.mock('./PageHosts', () => ({ PhonePages: () => <div data-testid="phone-pages" />, keptPageOf: (p: string) => (p === '/inbox' ? 'inbox' : p === '/todos' ? 'todos' : null) }))

import { AppShell } from './AppShell'
import { PageHeader } from './PageHeader'

const A = 'a'.repeat(32)
const client = new QueryClient()
const render = (ui: ReactNode) => rtlRender(<QueryClientProvider client={client}>{ui}</QueryClientProvider>)

beforeEach(() => {
  resetViewport()
  route.pathname = '/inbox'
  usePrefs.setState({ sidebarOpen: true, sidebarNodes: null, homeNoticeSeen: true })
  useUi.setState({ lastSessionsPath: null, leftDrawerOpen: false })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
})

describe('phone', () => {
  beforeEach(() => setViewport('phone'))

  it('has no docked sidebar and no tab bar; the inbox and the to-do list are kept by the shell, the rest go through the Outlet', () => {
    for (const path of ['/inbox', '/todos', '/n/aaaaaaaa/s/sess_1', '/n/aaaaaaaa', '/settings']) {
      route.pathname = path
      const { unmount } = render(<AppShell />)
      expect(screen.queryByTestId('sidebar-dock'), path).toBeNull()
      expect(screen.queryByTestId('sidebar'), path).toBeNull()
      expect(screen.queryByTestId('tab-bar'), path).toBeNull()
      expect(screen.getByTestId('phone-pages'), path).toBeInTheDocument()
      if (path === '/inbox' || path === '/todos') expect(screen.queryByTestId('outlet'), path).toBeNull()
      else expect(screen.getByTestId('outlet'), path).toBeInTheDocument()
      unmount()
    }
  })

  it('the menu button in the top bar opens the sidebar in a drawer, and picking a place closes it', async () => {
    const user = userEvent.setup()
    render(
      <>
        <PageHeader root crumbs={[{ label: 'Inbox' }]} showNodeStatus={false} />
        <AppShell />
      </>,
    )
    expect(screen.queryByTestId('sidebar')).toBeNull()
    await user.click(screen.getByTestId('menu-button'))
    expect(await screen.findByTestId('sidebar')).toBeInTheDocument()
    await user.click(screen.getByTestId('sidebar-inbox'))
    expect(useUi.getState().leftDrawerOpen).toBe(false)
  })

  it('shows the inbox count in the drawer', async () => {
    useSessions.getState().addPending(A, { request_id: 'r', session_id: 's', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    render(<AppShell />)
    act(() => useUi.getState().setLeftDrawer(true))
    expect(await screen.findByTestId('sidebar-inbox-count')).toHaveTextContent('1')
  })

  it('top bars: places in the sidebar get only the menu button, pages reached from them also get back', () => {
    const root = render(<PageHeader root crumbs={[{ label: 'Inbox' }]} />)
    expect(screen.getByTestId('menu-button')).toBeInTheDocument()
    expect(screen.queryByTestId('header-back')).toBeNull()
    root.unmount()
    const sub = render(<PageHeader crumbs={[{ label: 'Item' }]} />)
    expect(screen.getByTestId('menu-button')).toBeInTheDocument()
    expect(screen.getByTestId('header-back')).toBeInTheDocument()
    sub.unmount()
    // 页面自己要返回（文件预览回到树）时，`root` 也带返回
    render(<PageHeader root crumbs={[{ label: 'Files' }]} onBack={() => {}} />)
    expect(screen.getByTestId('header-back')).toBeInTheDocument()
  })
})

describe('tablet and desktop', () => {
  it('has the sidebar and the page, which always goes through the Outlet', () => {
    render(<AppShell />)
    expect(screen.getByTestId('sidebar')).toBeInTheDocument()
    expect(screen.getByTestId('outlet')).toBeInTheDocument()
    expect(screen.queryByTestId('phone-pages')).toBeNull()
  })

  it('closes a drawer left open from a narrow window', () => {
    useUi.setState({ leftDrawerOpen: true })
    render(<AppShell />)
    expect(useUi.getState().leftDrawerOpen).toBe(false)
  })

  it('records the last place in Sessions for “G S”, but not node settings or the inbox', () => {
    route.pathname = '/n/aaaaaaaa/s/sess_1'
    const { rerender: rr } = render(<AppShell />)
    const rerender = (ui: ReactNode): void => rr(<QueryClientProvider client={client}>{ui}</QueryClientProvider>)
    expect(useUi.getState().lastSessionsPath).toBe('/n/aaaaaaaa/s/sess_1')
    route.pathname = '/n/aaaaaaaa/settings'
    rerender(<AppShell />)
    route.pathname = '/inbox'
    rerender(<AppShell />)
    expect(useUi.getState().lastSessionsPath).toBe('/n/aaaaaaaa/s/sess_1')
    route.pathname = '/n/aaaaaaaa'
    rerender(<AppShell />)
    expect(useUi.getState().lastSessionsPath).toBe('/n/aaaaaaaa')
  })

  it('has the live region that announces the inbox count', () => {
    render(<AppShell />)
    expect(screen.getByTestId('inbox-live')).toHaveAttribute('aria-live', 'polite')
  })
})
