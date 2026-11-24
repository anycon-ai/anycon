// 规则页的入口（spec/web.md § 3.1、§ 3.3、§ 9.1）：左栏（手机上是抽屉）、命令面板，都只在有节点支持规则（`capabilities.rules`）时出现。
import type { ReactNode } from 'react'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
const route = vi.hoisted(() => ({ pathname: '/' }))
vi.mock('@tanstack/react-router', async (orig) => {
  const m = mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' })
  return { ...m, useRouterState: (o?: { select?: (s: unknown) => unknown }) => (o?.select ? o.select({ location: { pathname: route.pathname } }) : null) }
})
vi.mock('@/api/queries', () => ({ useMe: () => ({ data: undefined }), useDevices: () => ({ data: undefined }), logout: vi.fn() }))

import { CommandPalette } from './CommandPalette'
import { Sidebar } from './Sidebar'

const NODE = 'n'.repeat(32)
const info = (rules: boolean): NodeInfo => ({ capabilities: { rules } }) as unknown as NodeInfo
const wrap = (ui: ReactNode) => render(<QueryClientProvider client={new QueryClient()}>{ui}</QueryClientProvider>)
const setRules = (rules: boolean | undefined): void => useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'alpha', ...(rules === undefined ? {} : { info: info(rules) }) }) }, order: [NODE], loaded: true })

beforeEach(() => {
  navigate.mockReset()
  route.pathname = '/'
  usePrefs.setState({ sidebarOpen: true, sidebarNodes: null, sidebarRecent: true })
  useEnroll.setState({ items: {}, resolved: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
})

describe('the sidebar', () => {
  it('links to the rules page when a node supports rules', () => {
    setRules(true)
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-rules')).toHaveAttribute('href', '/rules')
  })

  it.each([[false], [undefined]])('has no entry when no node supports rules (%s)', (rules) => {
    setRules(rules)
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-rules')).toBeNull()
  })

  it('marks the entry as the current page on /rules', () => {
    setRules(true)
    route.pathname = '/rules'
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-rules')).toHaveAttribute('aria-current', 'page')
  })
})

describe('the command palette', () => {
  it('has a Rules item that opens the page, only when a node supports rules', async () => {
    const user = userEvent.setup()
    setRules(true)
    const { unmount } = wrap(<CommandPalette open onOpenChange={() => {}} />)
    await user.click(screen.getByText('Rules'))
    expect(navigate).toHaveBeenCalledWith({ to: '/rules' })
    unmount()
    setRules(false)
    wrap(<CommandPalette open onOpenChange={() => {}} />)
    expect(screen.queryByText('Rules')).toBeNull()
  })
})


describe('Twin navigation', () => {
  const setTwin = (twin: boolean): void => useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'alpha', info: { capabilities: { twin } } as unknown as NodeInfo }) }, order: [NODE], loaded: true })
  it('appears in sidebar, status bar and palette only with capability', async () => {
    setTwin(true)
    const first = wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-twin')).toHaveAttribute('href', '/twin')
    expect(screen.getByTestId('twin-status-bar')).toBeInTheDocument()
    first.unmount()
    const palette = wrap(<CommandPalette open onOpenChange={() => {}} />)
    await userEvent.click(screen.getByText('Twin'))
    expect(navigate).toHaveBeenCalledWith({ to: '/twin' })
    palette.unmount()
    setTwin(false)
    wrap(<><Sidebar nodeId={NODE} /><CommandPalette open onOpenChange={() => {}} /></>)
    expect(screen.queryByTestId('sidebar-twin')).not.toBeInTheDocument()
    expect(screen.queryByTestId('twin-status-bar')).not.toBeInTheDocument()
    expect(screen.queryByText('Twin')).not.toBeInTheDocument()
  })
})
