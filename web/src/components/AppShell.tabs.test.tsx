// 外壳在三种宽度下的导航（spec/web.md § 3）：手机没有左栏、底部标签栏只在标签页的根页面出现（进二级页收起、返回恢复）；
// 宽屏有左栏、没有标签栏。
import type { ReactNode } from 'react'
import { render as rtlRender, screen } from '@testing-library/react'
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
vi.mock('./TabHost', () => ({ PhoneTabs: () => <div data-testid="phone-tabs" /> }))

import { AppShell } from './AppShell'

const A = 'a'.repeat(32)
const client = new QueryClient()
const render = (ui: ReactNode) => rtlRender(<QueryClientProvider client={client}>{ui}</QueryClientProvider>)

beforeEach(() => {
  resetViewport()
  route.pathname = '/inbox'
  usePrefs.setState({ sidebarOpen: true, sidebarNodes: null, homeNoticeSeen: true })
  useUi.setState({ lastSessionsPath: null })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
})

describe('phone', () => {
  beforeEach(() => setViewport('phone'))

  it('has no sidebar (and no drawer), and shows the tab bar on a tab root page', () => {
    render(<AppShell />)
    expect(screen.queryByTestId('sidebar-dock')).toBeNull()
    expect(screen.queryByTestId('sidebar')).toBeNull()
    expect(screen.getByTestId('tab-bar')).toBeInTheDocument()
    expect(screen.getByTestId('phone-tabs')).toBeInTheDocument()
    // 标签页由常驻的 PhoneTabs 承载，不走 Outlet
    expect(screen.queryByTestId('outlet')).toBeNull()
  })

  it('shows it on every tab root and nowhere else: the session page, node home, settings hide it', () => {
    for (const path of ['/inbox', '/sessions', '/more']) {
      route.pathname = path
      const { unmount } = render(<AppShell />)
      expect(screen.queryByTestId('tab-bar'), path).toBeInTheDocument()
      unmount()
    }
    for (const path of ['/n/aaaaaaaa/s/sess_1', '/n/aaaaaaaa', '/n/aaaaaaaa/p/app', '/n/aaaaaaaa/settings', '/settings', '/n/aaaaaaaa/new']) {
      route.pathname = path
      const { unmount } = render(<AppShell />)
      expect(screen.queryByTestId('tab-bar'), path).toBeNull()
      expect(screen.getByTestId('outlet')).toBeInTheDocument()
      unmount()
    }
  })

  it('restores the bar when the user comes back to a tab root', () => {
    route.pathname = '/n/aaaaaaaa/s/sess_1'
    const { rerender: rr } = render(<AppShell />)
    const rerender = (ui: ReactNode): void => rr(<QueryClientProvider client={client}>{ui}</QueryClientProvider>)
    expect(screen.queryByTestId('tab-bar')).toBeNull()
    route.pathname = '/inbox'
    rerender(<AppShell />)
    expect(screen.getByTestId('tab-bar')).toBeInTheDocument()
  })

  it('shows the inbox count on the tab', () => {
    useSessions.getState().addPending(A, { request_id: 'r', session_id: 's', kind: 'tool', title: 't', detail: {}, options: [], created_at_ms: 1 })
    render(<AppShell />)
    expect(screen.getByTestId('tab-inbox-count')).toHaveTextContent('1')
  })
})

describe('tablet and desktop', () => {
  it('has the sidebar and the page, and no tab bar', () => {
    render(<AppShell />)
    expect(screen.getByTestId('sidebar')).toBeInTheDocument()
    expect(screen.getByTestId('outlet')).toBeInTheDocument()
    expect(screen.queryByTestId('tab-bar')).toBeNull()
    expect(screen.queryByTestId('phone-tabs')).toBeNull()
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
