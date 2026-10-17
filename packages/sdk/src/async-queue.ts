interface Waiter<T> {
  resolve: (r: IteratorResult<T>) => void
  reject: (e: unknown) => void
}

/**
 * 推入式异步队列，同时是 `AsyncIterableIterator`。
 * 生产者 `push` / `end` / `fail`；消费者 `for await`。消费者提前 `break`（调用 `return()`）时触发 `onDispose`。
 */
export class AsyncQueue<T> implements AsyncIterableIterator<T> {
  readonly #items: T[] = []
  readonly #waiters: Waiter<T>[] = []
  #done = false
  #error: { value: unknown } | undefined
  #onDispose: (() => void) | undefined

  constructor(onDispose?: () => void) {
    this.#onDispose = onDispose
  }

  /** 追加一个结束时的清理回调（已结束则立即调用）。 */
  onDispose(fn: () => void): void {
    if (this.#done) {
      fn()
      return
    }
    const prev = this.#onDispose
    this.#onDispose = prev
      ? () => {
          prev()
          fn()
        }
      : fn
  }

  get closed(): boolean {
    return this.#done
  }

  push(item: T): void {
    if (this.#done) return
    const waiter = this.#waiters.shift()
    if (waiter) waiter.resolve({ value: item, done: false })
    else this.#items.push(item)
  }

  /** 正常结束：已入队的元素仍会被消费完。 */
  end(): void {
    if (this.#done) return
    this.#done = true
    this.#dispose()
    for (const w of this.#waiters.splice(0)) w.resolve({ value: undefined, done: true })
  }

  /** 异常结束：已入队的元素消费完之后，下一次 `next()` 抛出 `error`。 */
  fail(error: unknown): void {
    if (this.#done) return
    this.#done = true
    this.#error = { value: error }
    this.#dispose()
    // 有等待者说明队列是空的：直接拒绝它们。否则错误留给下一次 next()。
    if (this.#waiters.length > 0) {
      const { value } = this.#error
      this.#error = undefined
      for (const w of this.#waiters.splice(0)) w.reject(value)
    }
  }

  next(): Promise<IteratorResult<T>> {
    if (this.#items.length > 0) {
      return Promise.resolve({ value: this.#items.shift() as T, done: false })
    }
    if (this.#error) {
      const { value } = this.#error
      this.#error = undefined
      return Promise.reject(value)
    }
    if (this.#done) return Promise.resolve({ value: undefined, done: true })
    return new Promise((resolve, reject) => this.#waiters.push({ resolve, reject }))
  }

  return(): Promise<IteratorResult<T>> {
    this.#items.length = 0
    this.#error = undefined
    this.#done = true
    this.#dispose()
    for (const w of this.#waiters.splice(0)) w.resolve({ value: undefined, done: true })
    return Promise.resolve({ value: undefined, done: true })
  }

  [Symbol.asyncIterator](): AsyncIterableIterator<T> {
    return this
  }

  #dispose(): void {
    const fn = this.#onDispose
    this.#onDispose = undefined
    fn?.()
  }
}
