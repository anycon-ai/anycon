// 可控的 IntersectionObserver（测试用）：jsdom 没有，test/setup.ts 里的占位永远不触发。
// 用法：`const io = installIntersectionObserver()`，渲染后 `io.intersect(element)` 让它「进入视口」；afterEach 里 `io.restore()`。
import { vi } from 'vitest'

export interface FakeIo {
  /** 让观察着这个元素的 observer 收到 isIntersecting。不传就让所有的都收到。 */
  intersect(el?: Element): void
  readonly observed: Element[]
  restore(): void
}

export function installIntersectionObserver(): FakeIo {
  const live = new Set<Fake>()
  class Fake {
    elements = new Set<Element>()
    constructor(private readonly cb: IntersectionObserverCallback) {
      live.add(this)
    }
    observe(el: Element): void {
      this.elements.add(el)
    }
    unobserve(el: Element): void {
      this.elements.delete(el)
    }
    disconnect(): void {
      this.elements.clear()
      live.delete(this)
    }
    fire(el: Element): void {
      this.cb([{ isIntersecting: true, target: el } as IntersectionObserverEntry], this as unknown as IntersectionObserver)
    }
  }
  vi.stubGlobal('IntersectionObserver', Fake)
  return {
    intersect(el) {
      for (const io of [...live]) for (const e of [...io.elements]) if (!el || e === el) io.fire(e)
    },
    get observed() {
      return [...live].flatMap((io) => [...io.elements])
    },
    restore() {
      vi.unstubAllGlobals()
    },
  }
}
