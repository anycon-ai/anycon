import '@testing-library/jest-dom/vitest'
import 'fake-indexeddb/auto'
import { cleanup } from '@testing-library/react'
import { afterEach } from 'vitest'
import '@/i18n'

// jsdom 缺的浏览器 API（Radix / 虚拟列表 / 媒体查询要用）
class NoopObserver {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
}
globalThis.ResizeObserver ??= NoopObserver as unknown as typeof ResizeObserver
globalThis.IntersectionObserver ??= NoopObserver as unknown as typeof IntersectionObserver
// jsdom 没有 URL.createObjectURL，vitest 里的是 Node 的版本、认不了 jsdom 的 Blob：换成可预期的假实现
let blobUrlSeq = 0
URL.createObjectURL = () => `blob:test/${++blobUrlSeq}`
URL.revokeObjectURL = () => {}
Element.prototype.hasPointerCapture ??= () => false
Element.prototype.releasePointerCapture ??= () => {}
Element.prototype.setPointerCapture ??= () => {}
Element.prototype.scrollIntoView ??= () => {}
if (!window.matchMedia) {
  window.matchMedia = ((query: string) => ({
    matches: /min-width/.test(query),
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia
}

afterEach(() => {
  cleanup()
})
