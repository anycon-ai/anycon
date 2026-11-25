// Bot 和规则的入口（spec/web.md § 3.1、§ 3.3、§ 9.1、§ 14.8）：左栏（手机上是抽屉）底部的一个 Bot 入口，
// 页面顶栏里 Bot / 规则两个标签；命令面板里各有一项。都只在有节点支持（`capabilities.twin` / `capabilities.rules`）时出现。
import type { ReactNode } from 'react'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { twinValue } from '@/test/fakeTwinDaemon'
import { mockRouter } from '@/test/routerMock'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { emptyTwin, useTwin } from '@/features/twin/store'

const navigate = vi.hoisted(() => vi.fn())
const route = vi.hoisted(() => ({ pathname: '/' }))
vi.mock('@tanstack/react-router', async (orig) => {
  const m = mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' })
  return { ...m, useRouterState: (o?: { select?: (s: unknown) => unknown }) => (o?.select ? o.select({ location: { pathname: route.pathname } }) : null) }
})
vi.mock('@/api/queries', () => ({ useMe: () => ({ data: undefined }), useDevices: () => ({ data: undefined }), logout: vi.fn() }))

import { BotTabs } from './BotTabs'
import { CommandPalette } from './CommandPalette'
import { Sidebar } from './Sidebar'

const NODE = 'n'.repeat(32)
const wrap = (ui: ReactNode) => render(<QueryClientProvider client={new QueryClient()}>{ui}</QueryClientProvider>)
const setCaps = (caps: { rules?: boolean; twin?: boolean } | undefined, conn: 'ready' | 'peer_offline' = 'ready'): void =>
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'alpha', conn, ...(caps === undefined ? {} : { info: { capabilities: caps } as unknown as NodeInfo }) }) }, order: [NODE], loaded: true })

beforeEach(() => {
  navigate.mockReset()
  route.pathname = '/'
  usePrefs.setState({ sidebarOpen: true, sidebarNodes: null, sidebarRecent: true })
  useEnroll.setState({ items: {}, resolved: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useTwin.getState().reset()
})

describe('the Bot entry at the bottom of the sidebar', () => {
  it('is the only way in: no Bot or Rules rows in the top navigation', () => {
    setCaps({ rules: true, twin: true })
    wrap(<Sidebar nodeId={NODE} />)
    const nav = screen.getByTestId('sidebar-nav')
    expect(nav).not.toHaveTextContent('Bot')
    expect(nav).not.toHaveTextContent('Rules')
    expect(screen.getByTestId('sidebar-bot')).toHaveAttribute('href', '/twin')
  })

  it('shows what the bot is doing on the ticked nodes', () => {
    setCaps({ rules: true, twin: true })
    useTwin.setState({ nodes: { [NODE]: { ...emptyTwin(), value: twinValue({ working: ['a', 'b'], waiting: ['c'], scope: { paused: true, projects: [], max_sessions: 1, daily_dispatches: 1 } }) } } })
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-bot-summary')).toHaveTextContent('2 working · 1 waiting for you · Paused')
  })

  it('says when a node is offline', () => {
    setCaps({ twin: true }, 'peer_offline')
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-bot-summary')).toHaveTextContent('Node offline')
  })

  it('goes to the rules page, named Rules, when no node supports the bot', () => {
    setCaps({ rules: true, twin: false })
    wrap(<Sidebar nodeId={NODE} />)
    const entry = screen.getByTestId('sidebar-bot')
    expect(entry).toHaveAttribute('href', '/rules')
    expect(entry).toHaveTextContent('Rules')
    expect(screen.queryByTestId('sidebar-bot-summary')).toBeNull()
  })

  it.each([[{ rules: false, twin: false }], [undefined]])('is absent when no node supports either (%o)', (caps) => {
    setCaps(caps)
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.queryByTestId('sidebar-bot')).toBeNull()
    expect(screen.queryByTestId('sidebar-bot-bar')).toBeNull()
  })

  it.each([['/twin'], ['/twin/nnnnnnnn'], ['/rules']])('is the current page on %s', (path) => {
    setCaps({ rules: true, twin: true })
    route.pathname = path
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-bot')).toHaveAttribute('aria-current', 'page')
  })

  it('is not the current page elsewhere', () => {
    setCaps({ rules: true, twin: true })
    route.pathname = '/inbox'
    wrap(<Sidebar nodeId={NODE} />)
    expect(screen.getByTestId('sidebar-bot')).not.toHaveAttribute('aria-current')
  })
})

describe('the Bot and Rules tabs', () => {
  it('links both pages and marks the current one', () => {
    setCaps({ rules: true, twin: true })
    wrap(<BotTabs current="rules" />)
    expect(screen.getByTestId('bot-tab-twin')).toHaveAttribute('href', '/twin')
    expect(screen.getByTestId('bot-tab-twin')).not.toHaveAttribute('aria-current')
    expect(screen.getByTestId('bot-tab-rules')).toHaveAttribute('href', '/rules')
    expect(screen.getByTestId('bot-tab-rules')).toHaveAttribute('aria-current', 'page')
  })

  it('leaves out a page no node supports, but always keeps the current one', () => {
    setCaps({ rules: true, twin: false })
    const { unmount } = wrap(<BotTabs current="rules" />)
    expect(screen.queryByTestId('bot-tab-twin')).toBeNull()
    unmount()
    wrap(<BotTabs current="twin" />)
    expect(screen.getByTestId('bot-tab-twin')).toHaveAttribute('aria-current', 'page')
    expect(screen.getByTestId('bot-tab-rules')).toBeInTheDocument()
  })
})

describe('the command palette', () => {
  it('has a Rules item that opens the page, only when a node supports rules', async () => {
    const user = userEvent.setup()
    setCaps({ rules: true })
    const { unmount } = wrap(<CommandPalette open onOpenChange={() => {}} />)
    await user.click(screen.getByText('Rules'))
    expect(navigate).toHaveBeenCalledWith({ to: '/rules' })
    unmount()
    setCaps({ rules: false })
    wrap(<CommandPalette open onOpenChange={() => {}} />)
    expect(screen.queryByText('Rules')).toBeNull()
  })

  it('has a Bot item that opens the page, only when a node supports the bot', async () => {
    setCaps({ twin: true })
    const palette = wrap(<CommandPalette open onOpenChange={() => {}} />)
    await userEvent.click(screen.getByText('Bot'))
    expect(navigate).toHaveBeenCalledWith({ to: '/twin' })
    palette.unmount()
    setCaps({ twin: false })
    wrap(<CommandPalette open onOpenChange={() => {}} />)
    expect(screen.queryByText('Bot')).not.toBeInTheDocument()
  })
})
