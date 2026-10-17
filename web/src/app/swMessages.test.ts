import { describe, expect, it, vi } from 'vitest'
import { NAVIGATE_MESSAGE } from '@/sw/click'
import { listenForSwMessages, navigateTargetOf, type MessageSource } from './swMessages'

function fakeSource() {
  let listener: ((e: MessageEvent<unknown>) => void) | null = null
  const source: MessageSource = {
    addEventListener: (_t, l) => void (listener = l),
    removeEventListener: (_t, l) => void (listener === l && (listener = null)),
  }
  return { source, send: (data: unknown, origin = 'https://hub.example') => listener?.({ data, origin } as MessageEvent<unknown>), hasListener: () => listener !== null }
}

describe('navigateTargetOf', () => {
  it('accepts only our message with an in-site absolute path', () => {
    expect(navigateTargetOf({ type: NAVIGATE_MESSAGE, url: '/n/abcd1234/s/sess_1' })).toBe('/n/abcd1234/s/sess_1')
    expect(navigateTargetOf({ type: NAVIGATE_MESSAGE, url: '/n/abcd1234/settings?tab=enroll&review=1' })).toBe('/n/abcd1234/settings?tab=enroll&review=1')
    for (const bad of [null, 'x', {}, { type: 'other', url: '/x' }, { type: NAVIGATE_MESSAGE }, { type: NAVIGATE_MESSAGE, url: 'https://evil.example/' }, { type: NAVIGATE_MESSAGE, url: '//evil.example' }, { type: NAVIGATE_MESSAGE, url: '/\\evil.example' }, { type: NAVIGATE_MESSAGE, url: 'javascript:alert(1)' }, { type: NAVIGATE_MESSAGE, url: '/\t/evil.example' }, { type: NAVIGATE_MESSAGE, url: '/\n/evil.example' }, { type: NAVIGATE_MESSAGE, url: '/\r\\evil.example' }]) {
      expect(navigateTargetOf(bad)).toBeNull()
    }
  })
})

describe('listenForSwMessages', () => {
  it('navigates in the page when the service worker asks (same-origin message)', () => {
    const { source, send } = fakeSource()
    const navigate = vi.fn()
    listenForSwMessages(navigate, source, 'https://hub.example')
    send({ type: NAVIGATE_MESSAGE, url: '/n/abcd1234/s/sess_1' })
    expect(navigate).toHaveBeenCalledExactlyOnceWith('/n/abcd1234/s/sess_1')
  })

  it('ignores a message from another origin and anything that is not ours', () => {
    const { source, send } = fakeSource()
    const navigate = vi.fn()
    listenForSwMessages(navigate, source, 'https://hub.example')
    send({ type: NAVIGATE_MESSAGE, url: '/x' }, 'https://evil.example')
    send({ type: 'hello' })
    send({ type: NAVIGATE_MESSAGE, url: '//evil.example' })
    send({ type: NAVIGATE_MESSAGE, url: '/\t/evil.example' })
    expect(navigate).not.toHaveBeenCalled()
  })

  it('stops listening when told to; without a service worker it does nothing', () => {
    const { source, send, hasListener } = fakeSource()
    const navigate = vi.fn()
    const stop = listenForSwMessages(navigate, source, 'https://hub.example')
    expect(hasListener()).toBe(true)
    stop()
    expect(hasListener()).toBe(false)
    send({ type: NAVIGATE_MESSAGE, url: '/x' })
    expect(navigate).not.toHaveBeenCalled()
    expect(() => listenForSwMessages(navigate, undefined, 'https://hub.example')()).not.toThrow()
  })
})
