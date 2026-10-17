// startTransport 把页面的可见性接到「后台满 30 秒断开 hub 连接」上（spec/web.md § 7）。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ClientSink } from '@/transport/client'

const h = vi.hoisted(() => ({
  sink: null as ClientSink | null,
  suspend: vi.fn(() => true),
  onVisible: vi.fn(async () => undefined),
}))

vi.mock('@/transport/instance', () => ({
  getClient: () => ({
    setSink: (s: ClientSink) => void (h.sink = s),
    start: async () => undefined,
    onVisible: h.onVisible,
    suspendForBackground: h.suspend,
    stop: () => undefined,
  }),
  rpc: () => ({ request: vi.fn(), isReady: () => false }),
}))

import { startTransport, stopTransport } from './bootstrap'

function setVisibility(v: DocumentVisibilityState): void {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => v })
  document.dispatchEvent(new Event('visibilitychange'))
}

beforeEach(() => {
  vi.useFakeTimers()
  h.suspend.mockReset()
  h.suspend.mockReturnValue(true)
  h.onVisible.mockClear()
})
afterEach(() => {
  stopTransport()
  Reflect.deleteProperty(document, 'visibilityState')
  vi.useRealTimers()
})

describe('startTransport — page visibility', () => {
  it('hidden for 30 s disconnects from the hub; coming back reconnects (onVisible)', async () => {
    await startTransport('acct-bg-1')
    setVisibility('hidden')
    await vi.advanceTimersByTimeAsync(29_999)
    expect(h.suspend).not.toHaveBeenCalled()
    await vi.advanceTimersByTimeAsync(1)
    expect(h.suspend).toHaveBeenCalledTimes(1)
    expect(h.onVisible).not.toHaveBeenCalled()
    setVisibility('visible')
    expect(h.onVisible).toHaveBeenCalledTimes(1)
  })

  it('back within 30 s: the connection is never touched', async () => {
    await startTransport('acct-bg-2')
    setVisibility('hidden')
    await vi.advanceTimersByTimeAsync(20_000)
    setVisibility('visible')
    await vi.advanceTimersByTimeAsync(5 * 60_000)
    expect(h.suspend).not.toHaveBeenCalled()
    expect(h.onVisible).toHaveBeenCalledTimes(1)
  })

  it('a page opened in a background tab counts from the moment the transport starts', async () => {
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' })
    await startTransport('acct-bg-3')
    await vi.advanceTimersByTimeAsync(30_000)
    expect(h.suspend).toHaveBeenCalledTimes(1)
  })

  it('stopping the transport (sign-out) cancels the pending disconnect and stops listening', async () => {
    await startTransport('acct-bg-4')
    setVisibility('hidden')
    stopTransport()
    await vi.advanceTimersByTimeAsync(60_000)
    expect(h.suspend).not.toHaveBeenCalled()
    setVisibility('visible')
    expect(h.onVisible).not.toHaveBeenCalled()
  })
})
