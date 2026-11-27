import { afterEach, describe, expect, it, vi } from 'vitest'
import { canVibrate, playChime, vibrateOnce } from './feedback'

const ANDROID = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Mobile Safari/537.36'
const IPHONE = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1'

afterEach(() => {
  Reflect.deleteProperty(navigator, 'userAgent')
  Reflect.deleteProperty(navigator, 'vibrate')
  vi.unstubAllGlobals()
})

describe('vibration', () => {
  it('is only available on Android with navigator.vibrate', () => {
    expect(canVibrate()).toBe(false)
    Object.defineProperty(navigator, 'userAgent', { configurable: true, value: IPHONE })
    expect(canVibrate()).toBe(false)
    Object.defineProperty(navigator, 'userAgent', { configurable: true, value: ANDROID })
    expect(canVibrate()).toBe(false)
    Object.defineProperty(navigator, 'vibrate', { configurable: true, value: () => true })
    expect(canVibrate()).toBe(true)
  })

  it('does nothing where it is not supported', () => {
    // 桌面浏览器有 API 但没有马达：不调
    const vibrate = vi.fn(() => true)
    Object.defineProperty(navigator, 'vibrate', { configurable: true, value: vibrate })
    vibrateOnce()
    expect(vibrate).not.toHaveBeenCalled()
  })
})

describe('chime', () => {
  it('is silent without WebAudio', () => {
    expect(() => playChime()).not.toThrow()
  })

  it('synthesises two short tones, no audio files', () => {
    const oscillators: { freq: number }[] = []
    class FakeContext {
      state = 'running'
      currentTime = 0
      destination = {}
      createOscillator() {
        const o = { type: '', frequency: { value: 0 }, connect: vi.fn(), start: vi.fn(), stop: vi.fn() }
        oscillators.push({
          get freq() {
            return o.frequency.value
          },
        })
        return o
      }
      createGain() {
        return { gain: { setValueAtTime: vi.fn(), exponentialRampToValueAtTime: vi.fn() }, connect: vi.fn() }
      }
      resume() {
        return Promise.resolve()
      }
    }
    vi.stubGlobal('AudioContext', FakeContext)
    playChime()
    expect(oscillators.map((o) => o.freq)).toEqual([880, 1318.5])
  })
})
