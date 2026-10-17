// 页面在后台满 30 秒就断开 hub 连接，回到前台再连（spec/web.md § 7）。
import { afterEach, describe, expect, it, vi } from 'vitest'
import { BACKGROUND_DISCONNECT_MS, BACKGROUND_RETRY_MS, createBackgroundWatcher } from './backgroundIdle'

afterEach(() => vi.useRealTimers())

function setup(opts: { busyTimes?: number } = {}) {
  vi.useFakeTimers()
  let visibility: DocumentVisibilityState = 'visible'
  let busy = opts.busyTimes ?? 0
  const client = {
    suspendForBackground: vi.fn(() => {
      if (busy > 0) {
        busy -= 1
        return false
      }
      return true
    }),
    onVisible: vi.fn(async () => undefined),
  }
  const watcher = createBackgroundWatcher({
    client: () => client,
    visibility: () => visibility,
    setTimer: (fn, ms) => setTimeout(fn, ms),
    clearTimer: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
  })
  const go = (v: DocumentVisibilityState) => {
    visibility = v
    watcher.onVisibilityChange()
  }
  return { client, watcher, go }
}

describe('page in the background', () => {
  it('disconnects after 30 s hidden, not before', () => {
    const { client, go } = setup()
    go('hidden')
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS - 1)
    expect(client.suspendForBackground).not.toHaveBeenCalled()
    vi.advanceTimersByTime(1)
    expect(client.suspendForBackground).toHaveBeenCalledTimes(1)
  })

  it('coming back within 30 s keeps the connection (and the timer starts over next time)', () => {
    const { client, go } = setup()
    go('hidden')
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS - 5000)
    go('visible')
    expect(client.onVisible).toHaveBeenCalledTimes(1)
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS * 2)
    expect(client.suspendForBackground).not.toHaveBeenCalled()
    // 再次进后台：重新数满 30 秒，不是接着上次剩下的 5 秒
    go('hidden')
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS - 1)
    expect(client.suspendForBackground).not.toHaveBeenCalled()
    vi.advanceTimersByTime(1)
    expect(client.suspendForBackground).toHaveBeenCalledTimes(1)
  })

  it('coming back after the disconnect asks the client to reconnect', () => {
    const { client, go } = setup()
    go('hidden')
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS)
    go('visible')
    expect(client.onVisible).toHaveBeenCalledTimes(1)
  })

  it('a second hidden event while already counting does not postpone the disconnect', () => {
    const { client, go } = setup()
    go('hidden')
    vi.advanceTimersByTime(20_000)
    go('hidden')
    vi.advanceTimersByTime(10_000)
    expect(client.suspendForBackground).toHaveBeenCalledTimes(1)
  })

  it('while a pairing / enrollment / request is in flight it tries again a little later, until it can', () => {
    const { client, go } = setup({ busyTimes: 2 })
    go('hidden')
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS)
    expect(client.suspendForBackground).toHaveBeenCalledTimes(1)
    vi.advanceTimersByTime(BACKGROUND_RETRY_MS)
    expect(client.suspendForBackground).toHaveBeenCalledTimes(2)
    vi.advanceTimersByTime(BACKGROUND_RETRY_MS)
    expect(client.suspendForBackground).toHaveBeenCalledTimes(3)
    vi.advanceTimersByTime(BACKGROUND_RETRY_MS * 4) // 断开成功之后不再试
    expect(client.suspendForBackground).toHaveBeenCalledTimes(3)
  })

  it('coming back while it is still waiting for the busy client cancels the retries', () => {
    const { client, go } = setup({ busyTimes: 99 })
    go('hidden')
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS)
    go('visible')
    vi.advanceTimersByTime(BACKGROUND_RETRY_MS * 4)
    expect(client.suspendForBackground).toHaveBeenCalledTimes(1)
  })

  it('a page that starts out hidden (opened in a background tab) counts too', () => {
    const { client, go, watcher } = setup()
    go('hidden')
    watcher.dispose()
    watcher.arm()
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS)
    expect(client.suspendForBackground).toHaveBeenCalledTimes(1)
  })

  it('arm() does nothing for a visible page; dispose() cancels a pending disconnect', () => {
    const { client, go, watcher } = setup()
    watcher.arm()
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS * 2)
    expect(client.suspendForBackground).not.toHaveBeenCalled()
    go('hidden')
    watcher.dispose()
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS * 2)
    expect(client.suspendForBackground).not.toHaveBeenCalled()
  })

  it('the timer fires but the page is visible again (an event we missed): nothing is disconnected', () => {
    vi.useFakeTimers()
    let visibility: DocumentVisibilityState = 'hidden'
    const client = { suspendForBackground: vi.fn(() => true), onVisible: vi.fn(async () => undefined) }
    const watcher = createBackgroundWatcher({ client: () => client, visibility: () => visibility, setTimer: (fn, ms) => setTimeout(fn, ms), clearTimer: (h) => clearTimeout(h as ReturnType<typeof setTimeout>) })
    watcher.onVisibilityChange()
    visibility = 'visible'
    vi.advanceTimersByTime(BACKGROUND_DISCONNECT_MS)
    expect(client.suspendForBackground).not.toHaveBeenCalled()
  })
})
