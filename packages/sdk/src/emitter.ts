type Handler = (...args: unknown[]) => void

/** 极简类型化事件发射器。监听器抛错不影响其他监听器。 */
export class Emitter<Events extends { [K in keyof Events]: unknown[] }> {
  readonly #handlers = new Map<keyof Events, Set<Handler>>()

  on<K extends keyof Events>(event: K, handler: (...args: Events[K]) => void): () => void {
    let set = this.#handlers.get(event)
    if (!set) {
      set = new Set()
      this.#handlers.set(event, set)
    }
    const h = handler as unknown as Handler
    set.add(h)
    return () => {
      set.delete(h)
    }
  }

  once<K extends keyof Events>(event: K, handler: (...args: Events[K]) => void): () => void {
    const off = this.on(event, (...args: Events[K]) => {
      off()
      handler(...args)
    })
    return off
  }

  protected emit<K extends keyof Events>(event: K, ...args: Events[K]): void {
    const set = this.#handlers.get(event)
    if (!set) return
    const handlers = Array.from(set)
    for (const h of handlers) {
      try {
        h(...args)
      } catch {
        // 监听器的异常不能拖垮连接状态机。
      }
    }
  }
}
