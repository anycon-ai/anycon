// 外观设置「禁止页面缩放」：iOS 和 Android 各自的做法，关掉后回到 index.html 原本的 viewport。
import { afterEach, beforeAll, describe, expect, it } from 'vitest'
import { applyZoomLock } from './zoom'

const ORIGINAL = 'width=device-width, initial-scale=1, viewport-fit=cover'
const meta = (): HTMLMetaElement => document.querySelector<HTMLMetaElement>('meta[name="viewport"]')!
const gesturePrevented = (type: string): boolean => {
  const e = new Event(type, { cancelable: true })
  document.dispatchEvent(e)
  return e.defaultPrevented
}

beforeAll(() => {
  const m = document.createElement('meta')
  m.name = 'viewport'
  m.content = ORIGINAL
  document.head.append(m)
})
afterEach(() => applyZoomLock(false, 'ios'))

describe('applyZoomLock', () => {
  it('off by default leaves the system behavior alone', () => {
    applyZoomLock(false, 'ios')
    expect(meta().content).toBe(ORIGINAL)
    expect(document.documentElement.dataset.zoomLock).toBeUndefined()
    expect(gesturePrevented('gesturestart')).toBe(false)
  })

  it('iOS: maximum-scale stops the focus zoom, pinch is blocked through gesture events, double-tap through touch-action', () => {
    applyZoomLock(true, 'ios')
    expect(meta().content).toBe(`${ORIGINAL}, maximum-scale=1`)
    expect(document.documentElement.dataset.zoomLock).toBe('1')
    expect(gesturePrevented('gesturestart')).toBe(true)
    expect(gesturePrevented('gesturechange')).toBe(true)

    applyZoomLock(false, 'ios')
    expect(meta().content).toBe(ORIGINAL)
    expect(document.documentElement.dataset.zoomLock).toBeUndefined()
    expect(gesturePrevented('gesturestart')).toBe(false)
  })

  it('Android: the viewport alone turns off pinch and double-tap zoom', () => {
    applyZoomLock(true, 'android')
    expect(meta().content).toBe(`${ORIGINAL}, maximum-scale=1, user-scalable=no`)
    expect(document.documentElement.dataset.zoomLock).toBeUndefined()
    expect(gesturePrevented('gesturestart')).toBe(false)
  })

  it('does nothing on desktop', () => {
    applyZoomLock(true, null)
    expect(meta().content).toBe(ORIGINAL)
    expect(document.documentElement.dataset.zoomLock).toBeUndefined()
  })

  it('applying twice does not stack the restrictions', () => {
    applyZoomLock(true, 'android')
    applyZoomLock(true, 'android')
    expect(meta().content).toBe(`${ORIGINAL}, maximum-scale=1, user-scalable=no`)
  })
})
