// /setup 里「要不要开通知」那一步（spec/web.md § 4.2）：直接开启（权限 → 订阅 → 上报 hub），可跳过；用不上的浏览器不问。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest'
import { resetDb } from '@/db'
import { useHub } from '@/stores/hub'
import { useUi } from '@/stores/ui'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { installFakePush, uninstallFakePush, VAPID_KEY, type FakePush } from '@/test/fakePush'
import { NotificationsStep } from './NotificationsStep'

let push: FakePush
let config: Record<string, unknown>
let onDone: Mock<() => void>

function mount(extra: Parameters<typeof installFakeApi>[0] = {}) {
  const api = installFakeApi({
    'GET /api/config': () => config,
    'GET /api/push/subscriptions': () => [],
    'POST /api/push/subscriptions': () => ({ id: 'sub1' }),
    'DELETE /api/push/subscriptions/*': () => undefined,
    ...extra,
  })
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <NotificationsStep onDone={onDone} />
    </QueryClientProvider>,
  )
  return api
}

function setDevice(id: string | null): void {
  useHub.setState({ snapshot: { phase: 'hub_authed', halt: null, attempt: 0, retryInMs: null, deviceId: id, paired: id !== null, deviceName: 'Chrome' } })
}

beforeEach(async () => {
  await resetDb()
  config = { mode: 'single', protocol_version: 1, version: '0.0.0', login_methods: ['token'], vapid_public_key: VAPID_KEY, push_available: true }
  push = installFakePush()
  setDevice('dev1')
  useUi.setState({ toasts: [] })
  onDone = vi.fn<() => void>()
})
afterEach(async () => {
  uninstallFakePush()
  await resetDb()
})

describe('setup: notifications step', () => {
  it('turning it on asks for permission, subscribes, reports the subscription to the hub and moves on', async () => {
    const user = userEvent.setup()
    const api = mount()
    const enable = await screen.findByTestId('setup-notify-enable')
    await waitFor(() => expect(enable).toBeEnabled())
    expect(onDone).not.toHaveBeenCalled()
    await user.click(enable)
    await waitFor(() => expect(onDone).toHaveBeenCalled())
    expect(push.permissionRequests).toBe(1)
    expect(push.subscribeCalls).toHaveLength(1)
    const posted = api.callsTo('POST /api/push/subscriptions')
    expect(posted).toHaveLength(1)
    expect(posted[0]!.body).toMatchObject({ device_id: 'dev1', endpoint: expect.stringContaining('https://fcm.googleapis.com/') })
  })

  it('shows only a quiet "checking" line while it is still reading, and never flashes the question for a browser that turns out not to need it', async () => {
    uninstallFakePush()
    push = installFakePush({ permission: 'denied' })
    const seen: string[] = []
    const observer = new MutationObserver(() => {
      if (document.querySelector('[data-testid="setup-notify"]')) seen.push('question')
    })
    observer.observe(document.body, { childList: true, subtree: true })
    try {
      mount()
      expect(screen.getByTestId('setup-notify-checking')).toBeInTheDocument()
      await waitFor(() => expect(onDone).toHaveBeenCalled())
      expect(seen).toEqual([])
      expect(screen.queryByTestId('setup-notify')).toBeNull()
    } finally {
      observer.disconnect()
    }
  })

  it('can be skipped: nothing is asked of the browser and nothing is sent to the hub', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('setup-notify-skip'))
    expect(onDone).toHaveBeenCalledTimes(1)
    expect(push.permissionRequests).toBe(0)
    expect(api.callsTo('POST /api/push/subscriptions')).toHaveLength(0)
  })

  it('closing the permission prompt without choosing keeps the step open (skip or try again) and says why', async () => {
    push.answer = 'default'
    const user = userEvent.setup()
    mount()
    const enable = await screen.findByTestId('setup-notify-enable')
    await waitFor(() => expect(enable).toBeEnabled())
    await user.click(enable)
    await waitFor(() => expect(useUi.getState().toasts.some((t) => t.kind === 'error')).toBe(true))
    expect(onDone).not.toHaveBeenCalled()
    expect(push.subscribeCalls).toHaveLength(0)
    expect(screen.getByTestId('setup-notify-enable')).toBeEnabled()
  })

  it('saying no at the permission prompt moves on (there is nothing more to ask), with the reason in a toast', async () => {
    push.answer = 'denied'
    const user = userEvent.setup()
    mount()
    const enable = await screen.findByTestId('setup-notify-enable')
    await waitFor(() => expect(enable).toBeEnabled())
    await user.click(enable)
    await waitFor(() => expect(onDone).toHaveBeenCalled())
    expect(useUi.getState().toasts.some((t) => t.kind === 'error')).toBe(true)
    expect(push.subscribeCalls).toHaveLength(0)
  })

  it('a hub that refuses the subscription leaves the step open and does not keep a half-made browser subscription', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/push/subscriptions': () => ({ status: 400, body: errorBody('push_endpoint_not_allowed') }) })
    const enable = await screen.findByTestId('setup-notify-enable')
    await waitFor(() => expect(enable).toBeEnabled())
    await user.click(enable)
    await waitFor(() => expect(useUi.getState().toasts.some((t) => t.kind === 'error')).toBe(true))
    expect(onDone).not.toHaveBeenCalled()
    expect(push.subscription).toBeNull()
  })

  it('does not offer to turn it on until this browser has a device id', async () => {
    setDevice(null)
    mount()
    const enable = await screen.findByTestId('setup-notify-enable')
    await new Promise((r) => setTimeout(r, 30))
    expect(enable).toBeDisabled()
  })

  it.each([
    ['plain http (no push at all)', () => { uninstallFakePush(); push = installFakePush({ secure: false }) }],
    ['notifications blocked in the browser', () => { uninstallFakePush(); push = installFakePush({ permission: 'denied' }) }],
    ['a hub without push keys', () => { config = { ...config, push_available: false } }],
    ['a browser that is already subscribed', () => { push.subscription = push.makeSubscription() }],
  ])('does not ask at all for %s: it goes straight on', async (_name, arrange) => {
    arrange()
    mount()
    await waitFor(() => expect(onDone).toHaveBeenCalled())
    expect(screen.queryByTestId('setup-notify')).toBeNull()
    expect(push.permissionRequests).toBe(0)
  })
})
