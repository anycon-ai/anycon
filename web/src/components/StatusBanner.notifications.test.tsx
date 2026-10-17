// § 3.4 第 5 条：「这个浏览器尚未启用通知」——浏览器能收推送、配对过节点、本浏览器没有订阅、没关过才显示；
// 点「去开启」去 /settings/notifications，可关闭，关了记 localStorage、不再显示。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { installFakeApi } from '@/test/fakeApi'
import { installFakePush, uninstallFakePush, VAPID_KEY, type FakePush } from '@/test/fakePush'
import { nodeEntry } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useDrafts } from '@/stores/drafts'
import { useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const h = vi.hoisted(() => ({ navigate: vi.fn(), path: '/n/aaaaaaaa' }))
vi.mock('@tanstack/react-router', async (orig) => ({
  ...mockRouter(await orig<object>(), h.navigate, {}),
  // 没有真路由：当前路径由测试设
  useRouterState: ({ select }: { select: (s: { location: { pathname: string } }) => unknown }) => select({ location: { pathname: h.path } }),
}))
vi.mock('@/transport/instance', () => ({ getClient: () => ({ forceReconnect: vi.fn(), takeOver: vi.fn() }) }))

import { StatusBanner } from './StatusBanner'

const A = 'a'.repeat(32)
let push: FakePush
let config: Record<string, unknown>

const banner = () => screen.queryByTestId('status-banner')

function mount(nodeConn: 'ready' | 'peer_offline' = 'ready') {
  installFakeApi({ 'GET /api/config': () => config })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return render(
    <QueryClientProvider client={queryClient}>
      <StatusBanner nodeId={A} nodeConn={nodeConn} />
    </QueryClientProvider>,
  )
}

/** 等到横幅出现（它要先读配置和订阅，是异步的）。 */
const shown = () => screen.findByTestId('status-banner')
/** 读完配置和订阅之后仍然没有横幅：给异步读取留一点时间再断言。 */
async function stillHidden(): Promise<void> {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 30))
  })
  expect(banner()).toBeNull()
}

beforeEach(() => {
  h.navigate.mockReset()
  h.path = '/n/aaaaaaaa'
  config = { mode: 'single', protocol_version: 1, version: '0.0.0', login_methods: ['token'], vapid_public_key: VAPID_KEY, push_available: true }
  push = installFakePush()
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null, scannerOpen: false, securityTick: {} })
  useHub.setState({ started: true, snapshot: { ...useHub.getState().snapshot, phase: 'hub_authed', halt: null, attempt: 0 } })
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useUi.setState({ updateReady: false, applyUpdate: null, storageError: false })
  usePrefs.setState({ notifyBannerDismissed: false })
})
afterEach(() => uninstallFakePush())

describe('"notifications are not enabled in this browser" banner', () => {
  it('shows for a browser that can do push, has a paired node and no subscription yet', async () => {
    mount()
    expect(await shown()).toHaveAttribute('data-kind', 'notifications')
    expect(banner()).toHaveTextContent(/not enabled/i)
  })

  it('"Turn on" goes to the notification settings and counts as having been asked', async () => {
    const user = userEvent.setup()
    mount()
    await shown()
    await user.click(screen.getByTestId('banner-notifications-enable'))
    expect(h.navigate).toHaveBeenCalledWith({ to: '/settings/notifications' })
    expect(usePrefs.getState().notifyBannerDismissed).toBe(true)
    expect(banner()).toBeNull()
  })

  it('can be closed, and the choice is remembered in localStorage so it never comes back', async () => {
    const user = userEvent.setup()
    const first = mount()
    await shown()
    await user.click(screen.getByTestId('banner-notifications-dismiss'))
    expect(banner()).toBeNull()
    expect(JSON.parse(localStorage.getItem('anycon.prefs') ?? '{}').state.notifyBannerDismissed).toBe(true)
    first.unmount()
    mount() // 重新打开页面
    await stillHidden()
  })

  it('is not shown when this browser already has a subscription', async () => {
    push.subscription = push.makeSubscription()
    mount()
    await stillHidden()
  })

  it('is not shown for a browser that cannot receive push (plain http, no PushManager), or when the user blocked notifications', async () => {
    uninstallFakePush()
    push = installFakePush({ secure: false })
    const a = mount()
    await stillHidden()
    a.unmount()
    uninstallFakePush()
    push = installFakePush({ permission: 'denied' })
    mount()
    await stillHidden()
  })

  it('is not shown before any node is paired, or when the hub has no push keys', async () => {
    useNodes.setState({ byId: {}, order: [], loaded: true })
    const a = mount()
    await stillHidden()
    a.unmount()
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A }) }, order: [A], loaded: true })
    config = { ...config, push_available: false }
    mount()
    await stillHidden()
  })

  it('is not shown without a service worker registration (dev server): there would be nothing to turn on', async () => {
    uninstallFakePush()
    push = installFakePush({ registered: false })
    mount()
    await stillHidden()
  })

  it('does not nag on the notification settings page itself, and comes back elsewhere if nothing was turned on', async () => {
    h.path = '/settings/notifications'
    const view = mount()
    await stillHidden()
    h.path = '/n/aaaaaaaa'
    view.rerender(
      <QueryClientProvider client={new QueryClient()}>
        <StatusBanner nodeId={A} nodeConn="ready" />
      </QueryClientProvider>,
    )
    expect(await shown()).toHaveAttribute('data-kind', 'notifications')
  })

  it('does not flash after push was turned on in the settings page and the user leaves it', async () => {
    h.path = '/settings/notifications'
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    installFakeApi({ 'GET /api/config': () => config })
    const view = render(
      <QueryClientProvider client={queryClient}>
        <StatusBanner nodeId={A} nodeConn="ready" />
      </QueryClientProvider>,
    )
    await act(async () => {
      await new Promise((r) => setTimeout(r, 30))
    })
    push.subscription = push.makeSubscription() // 用户在设置页里开了推送
    h.path = '/n/aaaaaaaa'
    view.rerender(
      <QueryClientProvider client={queryClient}>
        <StatusBanner nodeId={A} nodeConn="ready" />
      </QueryClientProvider>,
    )
    // 离开设置页的那一刻（订阅状态还没重新读完）就不能用旧结果亮一下横幅
    expect(banner()).toBeNull()
    await stillHidden()
  })

  it('has the priority of § 3.4: below "node offline" and unsent messages, above "new version"', async () => {
    useUi.setState({ updateReady: true })
    mount()
    expect(banner()).toHaveAttribute('data-kind', 'update') // 通知横幅还没读完订阅，先是「新版本」
    await waitFor(() => expect(banner()).toHaveAttribute('data-kind', 'notifications'))
    await act(async () => useUi.getState().setStorageError(true))
    await waitFor(() => expect(banner()).toHaveAttribute('data-kind', 'storage'))
    await act(async () => useUi.getState().setStorageError(false))
    await waitFor(() => expect(banner()).toHaveAttribute('data-kind', 'notifications'))
    usePrefs.setState({ notifyBannerDismissed: true })
    await waitFor(() => expect(banner()).toHaveAttribute('data-kind', 'update'))
  })
})
