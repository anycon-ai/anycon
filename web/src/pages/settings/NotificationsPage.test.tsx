// /settings/notifications（spec/web.md § 4.9）：本浏览器的推送、每个类别的开关、其他浏览器的订阅。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { resetDb } from '@/db'
import { useHub } from '@/stores/hub'
import { useUi } from '@/stores/ui'
import { errorBody, installFakeApi, type ApiHandler } from '@/test/fakeApi'
import { installFakePush, uninstallFakePush, VAPID_KEY, type FakePush } from '@/test/fakePush'
import { mockRouter } from '@/test/routerMock'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

import { NotificationsPage } from './NotificationsPage'

interface PrefRow {
  category: string
  push: boolean
  email: boolean
}
interface Hub {
  config: Record<string, unknown>
  me: Record<string, unknown>
  prefs: PrefRow[]
  emailAvailable: boolean
  subs: Array<{ id: string; device_id: string; device_name?: string; endpoint_host: string; created_at_ms: number }>
  devices: Array<Record<string, unknown>>
}

const DEFAULT_PREFS: PrefRow[] = [
  { category: 'permission_requested', push: true, email: true },
  { category: 'error', push: true, email: true },
  { category: 'enroll_requested', push: true, email: true },
  { category: 'security', push: true, email: true },
  { category: 'turn_finished', push: true, email: false },
  { category: 'session_idle', push: false, email: false },
]

let hub: Hub
let push: FakePush

const toasts = () => useUi.getState().toasts.map((t) => `${t.kind}:${t.text}`)

function mount(over: Record<string, ApiHandler> = {}) {
  const api = installFakeApi({
    'GET /api/config': () => hub.config,
    'GET /api/me': () => hub.me,
    'GET /api/me/notifications': () => ({ categories: hub.prefs, email_available: hub.emailAvailable, push_available: true }),
    'PUT /api/me/notifications': (c) => {
      hub.prefs = (c.body as { categories: PrefRow[] }).categories
      return undefined
    },
    'GET /api/push/subscriptions': () => hub.subs,
    'POST /api/push/subscriptions': (c) => {
      const b = c.body as { device_id: string; endpoint: string }
      hub.subs = [...hub.subs.filter((s) => s.device_id !== b.device_id), { id: `sub-${b.device_id}`, device_id: b.device_id, endpoint_host: new URL(b.endpoint).hostname, created_at_ms: 1 }]
      return { id: `sub-${b.device_id}` }
    },
    'DELETE /api/push/subscriptions/*': (c) => {
      const id = c.path.split('/').pop()
      hub.subs = hub.subs.filter((s) => s.id !== id)
      return undefined
    },
    'POST /api/push/test': () => ({ result: 'ok' }),
    'GET /api/devices': () => hub.devices,
    ...over,
  })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <NotificationsPage />
    </QueryClientProvider>,
  )
  return api
}

function setDevice(id: string | null): void {
  useHub.setState({ snapshot: { phase: 'hub_authed', halt: null, attempt: 0, retryInMs: null, deviceId: id, paired: id !== null, deviceName: 'Chrome' } })
}

beforeEach(async () => {
  await resetDb()
  hub = {
    config: { mode: 'single', protocol_version: 1, version: '0.0.0', login_methods: ['token', 'email'], vapid_public_key: VAPID_KEY, push_available: true },
    me: { id: 'u1', display_name: 'Ada', is_admin: false, email: 'me@example.com', login_methods: ['token', 'email'] },
    prefs: DEFAULT_PREFS.map((p) => ({ ...p })),
    emailAvailable: true,
    subs: [],
    devices: [
      { id: 'dev1', name: 'This Chrome', kind: 'web', platform: 'macos', created_at_ms: 1, online: true, paired_with: [] },
      { id: 'dev2', name: 'Pixel phone', kind: 'web', platform: 'android', created_at_ms: 1, online: false, paired_with: [] },
    ],
  }
  push = installFakePush()
  setDevice('dev1')
  useUi.setState({ toasts: [] })
})
afterEach(async () => {
  uninstallFakePush()
  await resetDb()
})

const status = () => screen.findByTestId('push-status')

describe('push on this browser — states', () => {
  it('not supported over plain http: says so and offers nothing', async () => {
    uninstallFakePush()
    push = installFakePush({ secure: false })
    mount()
    expect((await status()).getAttribute('data-state')).toBe('unsupported')
    expect(screen.getByTestId('push-note')).toHaveTextContent(/secure connection|安全连接/)
    expect(screen.queryByTestId('push-enable')).toBeNull()
  })

  it('iPhone in a Safari tab: how to add the app to the Home Screen', async () => {
    Object.defineProperty(navigator, 'userAgent', { configurable: true, value: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Safari/604.1' })
    Reflect.deleteProperty(window, 'PushManager')
    Reflect.deleteProperty(globalThis, 'PushManager')
    mount()
    expect((await status()).getAttribute('data-state')).toBe('unsupported')
    expect(screen.getByTestId('push-note')).toHaveTextContent(/Home Screen|主屏幕/)
    Reflect.deleteProperty(navigator, 'userAgent')
  })

  it('a hub without push keys', async () => {
    hub.config = { ...hub.config, push_available: false, vapid_public_key: null }
    mount()
    await waitFor(() => expect(screen.getByTestId('push-status').getAttribute('data-state')).toBe('unavailable'))
    expect(screen.getByTestId('push-note')).toHaveTextContent(/no push keys|没有推送密钥/)
    expect(screen.queryByTestId('push-enable')).toBeNull()
  })

  it('blocked by the browser: explains where to change it, offers nothing', async () => {
    push.permission = 'denied'
    mount()
    await waitFor(() => expect(screen.getByTestId('push-status').getAttribute('data-state')).toBe('denied'))
    expect(screen.getByTestId('push-note')).toHaveTextContent(/site settings|网站设置/)
    expect(screen.queryByTestId('push-enable')).toBeNull()
  })

  it('a browser that never paired has no device id: Turn on is disabled and the note says to pair or unlock a node first', async () => {
    setDevice(null)
    mount()
    await waitFor(() => expect(screen.getByTestId('push-status').getAttribute('data-state')).toBe('off'))
    expect(screen.getByTestId('push-enable')).toBeDisabled()
    expect(screen.getByTestId('push-note')).toHaveTextContent(/Pair or unlock a node|配对或解锁一台节点/)
  })

  it('the device id also comes from the stored identity when the hub connection is not up yet', async () => {
    const { putIdentity } = await import('@/db')
    await putIdentity({ id: 'self', device_id: 'dev1', static_keypair: { public: new Uint8Array(32), secret: new Uint8Array(32) }, device_name: 'Chrome' })
    setDevice(null)
    mount()
    await waitFor(() => expect(screen.getByTestId('push-enable')).toBeEnabled())
  })

  it('a browser that already has a subscription starts as On', async () => {
    push.permission = 'granted'
    push.subscription = push.makeSubscription()
    hub.subs = [{ id: 'sub-dev1', device_id: 'dev1', endpoint_host: 'fcm.googleapis.com', created_at_ms: 1 }]
    mount()
    await waitFor(() => expect(screen.getByTestId('push-status').getAttribute('data-state')).toBe('on'))
    expect(screen.getByTestId('push-disable')).toBeInTheDocument()
    expect(screen.getByTestId('push-test')).toBeInTheDocument()
    expect(screen.queryByTestId('push-enable')).toBeNull()
  })
})

describe('push on this browser — turning on and off', () => {
  it('Turn on: asks the browser, subscribes, reports to the hub with this device id, and flips to On', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('push-enable'))
    await waitFor(() => expect(screen.getByTestId('push-status').getAttribute('data-state')).toBe('on'))
    expect(push.permissionRequests).toBe(1)
    expect(push.subscribeCalls[0]!.userVisibleOnly).toBe(true)
    const posted = api.callsTo('POST /api/push/subscriptions')
    expect(posted).toHaveLength(1)
    expect(posted[0]!.body).toMatchObject({ device_id: 'dev1', keys: { p256dh: 'P256DH_KEY', auth: 'AUTH_KEY' } })
    expect(toasts().join('|')).toMatch(/success:.*(Push is on|推送已开启)/)
  })

  it('Turn on reports once even though the list of subscriptions the page holds was read before (the page-open check must not fire again)', async () => {
    const user = userEvent.setup()
    // 这份列表永远是空的（开启之前读的那份、刷新慢了的样子）：对账如果还在，会把刚报过的又报一遍
    const api = mount({ 'GET /api/push/subscriptions': () => [] })
    await user.click(await screen.findByTestId('push-enable'))
    await waitFor(() => expect(screen.getByTestId('push-status').getAttribute('data-state')).toBe('on'))
    await new Promise((r) => setTimeout(r, 80))
    expect(api.callsTo('POST /api/push/subscriptions')).toHaveLength(1)
  })

  it('the user says no at the permission prompt: stays off (now blocked), explains, nothing reported', async () => {
    push.answer = 'denied'
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('push-enable'))
    await waitFor(() => expect(screen.getByTestId('push-status').getAttribute('data-state')).toBe('denied'))
    expect(toasts().join('|')).toMatch(/error:.*(blocked|拦截)/)
    expect(api.callsTo('POST /api/push/subscriptions')).toHaveLength(0)
  })

  it('the hub refuses the push service: shown in plain words, the browser subscription is rolled back, still Off', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/push/subscriptions': () => ({ status: 400, body: errorBody('push_endpoint_not_allowed') }) })
    await user.click(await screen.findByTestId('push-enable'))
    await waitFor(() => expect(toasts().join('|')).toMatch(/does not support this browser’s push service|不支持这个浏览器的推送服务/))
    expect(push.subscription).toBeNull()
    expect(screen.getByTestId('push-status').getAttribute('data-state')).toBe('off')
    expect(screen.getByTestId('push-enable')).toBeEnabled()
  })

  it('Turn off: unsubscribes in the browser and removes this device’s row at the hub', async () => {
    push.permission = 'granted'
    const sub = push.makeSubscription()
    push.subscription = sub
    hub.subs = [{ id: 'sub-dev1', device_id: 'dev1', endpoint_host: 'fcm.googleapis.com', created_at_ms: 1 }]
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('push-disable'))
    await waitFor(() => expect(screen.getByTestId('push-status').getAttribute('data-state')).toBe('off'))
    expect(sub.unsubscribe).toHaveBeenCalledTimes(1)
    expect(api.callsTo('DELETE /api/push/subscriptions/sub-dev1')).toHaveLength(1)
    expect(hub.subs).toEqual([])
  })

  it('opening the page re-reports a subscription the hub lost (once)', async () => {
    push.permission = 'granted'
    push.subscription = push.makeSubscription()
    hub.subs = []
    const api = mount()
    await waitFor(() => expect(api.callsTo('POST /api/push/subscriptions')).toHaveLength(1))
    expect(api.callsTo('POST /api/push/subscriptions')[0]!.body).toMatchObject({ device_id: 'dev1' })
    await new Promise((r) => setTimeout(r, 50))
    expect(api.callsTo('POST /api/push/subscriptions')).toHaveLength(1)
  })
})

describe('push on this browser — test push', () => {
  beforeEach(() => {
    push.permission = 'granted'
    push.subscription = push.makeSubscription()
    hub.subs = [{ id: 'sub-dev1', device_id: 'dev1', endpoint_host: 'fcm.googleapis.com', created_at_ms: 1 }]
  })

  it('asks the hub to push to this device and says it was sent', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('push-test'))
    await waitFor(() => expect(toasts().join('|')).toMatch(/success:.*(Test push sent|测试推送已发出)/))
    expect(api.callsTo('POST /api/push/test')[0]!.body).toEqual({ device_id: 'dev1' })
  })

  it.each([
    ['gone', /no longer knows this browser|已经不认识这个浏览器/],
    ['error', /did not accept the test push|没有接受这条测试推送/],
  ])('the push service answers "%s"', async (result, text) => {
    const user = userEvent.setup()
    mount({ 'POST /api/push/test': () => ({ result }) })
    await user.click(await screen.findByTestId('push-test'))
    await waitFor(() => expect(toasts().join('|')).toMatch(text))
  })

  it('"error" without a status (the push service could not be reached) does not invent one', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/push/test': () => ({ result: 'error' }) })
    await user.click(await screen.findByTestId('push-test'))
    await waitFor(() => expect(toasts().join('|')).toMatch(/did not accept the test push|没有接受这条测试推送/))
    expect(toasts().join('|')).not.toMatch(/HTTP/)
  })

  it('"error" shows the status code the push service answered with', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/push/test': () => ({ result: 'error', status: 403 }) })
    await user.click(await screen.findByTestId('push-test'))
    await waitFor(() => expect(toasts().join('|')).toMatch(/error:.*did not accept the test push.*HTTP 403|error:.*没有接受这条测试推送.*HTTP 403/))
  })

  it('an HTTP error from the hub is shown too', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/push/test': () => ({ status: 404, body: errorBody('device_not_found') }) })
    await user.click(await screen.findByTestId('push-test'))
    await waitFor(() => expect(toasts().join('|')).toMatch(/does not know this browser|不认识这个浏览器/))
  })
})

describe('what to notify — categories', () => {
  it('lists the six categories in order with their current push / email switches', async () => {
    mount()
    const rows = await screen.findAllByTestId(/^notify-row-/)
    expect(rows.map((r) => r.getAttribute('data-testid'))).toEqual(['permission_requested', 'error', 'enroll_requested', 'security', 'turn_finished', 'session_idle'].map((c) => `notify-row-${c}`))
    expect(screen.getByTestId('notify-push-permission_requested')).toBeChecked()
    expect(screen.getByTestId('notify-push-session_idle')).not.toBeChecked()
    expect(screen.getByTestId('notify-email-error')).toBeChecked()
    expect(screen.getByTestId('notify-email-turn_finished')).not.toBeChecked()
  })

  it('flipping a switch PUTs the whole list with that one change', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('notify-push-session_idle'))
    await waitFor(() => expect(api.callsTo('PUT /api/me/notifications')).toHaveLength(1))
    const sent = (api.callsTo('PUT /api/me/notifications')[0]!.body as { categories: PrefRow[] }).categories
    expect(sent).toHaveLength(6)
    expect(sent.find((c) => c.category === 'session_idle')).toEqual({ category: 'session_idle', push: true, email: false })
    expect(sent.filter((c) => c.category !== 'session_idle')).toEqual(DEFAULT_PREFS.filter((c) => c.category !== 'session_idle'))
    await waitFor(() => expect(screen.getByTestId('notify-push-session_idle')).toBeChecked())

    await user.click(screen.getByTestId('notify-email-security'))
    await waitFor(() => expect(api.callsTo('PUT /api/me/notifications')).toHaveLength(2))
    const second = (api.callsTo('PUT /api/me/notifications')[1]!.body as { categories: PrefRow[] }).categories
    expect(second.find((c) => c.category === 'security')).toEqual({ category: 'security', push: true, email: false })
    expect(second.find((c) => c.category === 'session_idle')?.push).toBe(true) // 上一次的改动还在
  })

  it('a rejected save puts the switch back and says why', async () => {
    const user = userEvent.setup()
    mount({ 'PUT /api/me/notifications': () => ({ status: 400, body: errorBody('bad_request') }) })
    await user.click(await screen.findByTestId('notify-push-session_idle'))
    await waitFor(() => expect(toasts().join('|')).toMatch(/error:.*(Could not save|没能保存)/))
    await waitFor(() => expect(screen.getByTestId('notify-push-session_idle')).not.toBeChecked())
  })

  it('email switches are off-limits for the two categories that never send email', async () => {
    mount()
    await screen.findAllByTestId(/^notify-row-/)
    expect(screen.getByTestId('notify-email-turn_finished')).toBeDisabled()
    expect(screen.getByTestId('notify-email-session_idle')).toBeDisabled()
    expect(screen.getByTestId('notify-email-error')).toBeEnabled()
  })

  it('email not available because the hub has no SMTP: the whole column is disabled, with the reason', async () => {
    hub.emailAvailable = false
    hub.config = { ...hub.config, login_methods: ['token'] }
    mount()
    await screen.findAllByTestId(/^notify-row-/)
    await waitFor(() => expect(screen.getByTestId('notify-email-why').getAttribute('data-why')).toBe('smtp'))
    for (const c of ['permission_requested', 'error', 'enroll_requested', 'security']) expect(screen.getByTestId(`notify-email-${c}`)).toBeDisabled()
    expect(screen.getByTestId('notify-push-error')).toBeEnabled() // 推送不受影响
  })

  it('email not available because the account has no email: the reason links to the profile', async () => {
    hub.emailAvailable = false
    hub.me = { ...hub.me, email: null }
    mount()
    await screen.findAllByTestId(/^notify-row-/)
    const why = await screen.findByTestId('notify-email-why')
    await waitFor(() => expect(why.getAttribute('data-why')).toBe('bind'))
    expect(within(why).getByRole('link')).toHaveAttribute('href', '/settings')
    expect(screen.getByTestId('notify-email-error')).toBeDisabled()
  })

  it('a hub that fails to return the settings says so', async () => {
    mount({ 'GET /api/me/notifications': () => ({ status: 500, body: errorBody('internal') }) })
    expect(await screen.findByTestId('notify-load-failed')).toBeInTheDocument()
  })
})

describe('other browsers with push', () => {
  it('lists the account’s other subscriptions (device name, push service host), not this browser’s own', async () => {
    hub.subs = [
      { id: 'sub-dev1', device_id: 'dev1', endpoint_host: 'fcm.googleapis.com', created_at_ms: Date.now() },
      { id: 'sub-dev2', device_id: 'dev2', endpoint_host: 'updates.push.services.mozilla.com', created_at_ms: Date.now() - 86_400_000 },
      { id: 'sub-gone', device_id: 'dev9', endpoint_host: 'web.push.apple.com', created_at_ms: Date.now() },
    ]
    mount()
    const rows = await screen.findAllByTestId('other-browser-row')
    expect(rows).toHaveLength(2)
    await waitFor(() => expect(rows[0]).toHaveTextContent('Pixel phone'))
    expect(rows[0]).toHaveTextContent('updates.push.services.mozilla.com')
    expect(rows[1]).toHaveTextContent(/Unknown browser|未知浏览器/)
  })

  it('prefers the device name the hub sends along with the subscription', async () => {
    hub.subs = [{ id: 'sub-dev2', device_id: 'dev2', device_name: 'Pixel (from the hub)', endpoint_host: 'fcm.googleapis.com', created_at_ms: Date.now() }]
    mount({ 'GET /api/devices': () => ({ status: 500, body: errorBody('internal') }) })
    expect(await screen.findByTestId('other-browser-row')).toHaveTextContent('Pixel (from the hub)')
  })

  it('says so when there are none', async () => {
    mount()
    expect(await screen.findByTestId('other-browsers-empty')).toBeInTheDocument()
  })

  it('Remove deletes that subscription at the hub and the row disappears', async () => {
    hub.subs = [{ id: 'sub-dev2', device_id: 'dev2', endpoint_host: 'updates.push.services.mozilla.com', created_at_ms: Date.now() }]
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('other-browser-remove'))
    await waitFor(() => expect(api.callsTo('DELETE /api/push/subscriptions/sub-dev2')).toHaveLength(1))
    await waitFor(() => expect(screen.queryByTestId('other-browser-row')).toBeNull())
  })
})
