/** 极简异步邮箱：按类型等待事件，带超时。配对流程用它把回调式的控制消息串成顺序代码。 */
export class MailboxClosed extends Error {}
export class MailboxTimeout extends Error {}
/** `cancelWait()` 撤掉了正在等的那次 `next`（调用方已经不需要它的结果）。 */
export class MailboxCancelled extends Error {}

export class Mailbox<T extends { t: string }> {
  private readonly queue: T[] = []
  private waiter: { types: string[]; resolve: (v: T) => void; reject: (e: Error) => void; timer: ReturnType<typeof setTimeout> } | null = null
  private closed = false

  push(item: T): void {
    if (this.closed) return
    if (this.waiter && this.waiter.types.includes(item.t)) {
      const w = this.waiter
      this.waiter = null
      clearTimeout(w.timer)
      w.resolve(item)
      return
    }
    this.queue.push(item)
  }

  close(): void {
    this.closed = true
    if (this.waiter) {
      const w = this.waiter
      this.waiter = null
      clearTimeout(w.timer)
      w.reject(new MailboxClosed('closed'))
    }
  }

  /** 撤掉当前正在等的 `next`（它的 Promise 以 `MailboxCancelled` 结束），不吞事件：之后到的事件照常进队列。 */
  cancelWait(): void {
    if (!this.waiter) return
    const w = this.waiter
    this.waiter = null
    clearTimeout(w.timer)
    w.reject(new MailboxCancelled('cancelled'))
  }

  /** 等待下一个类型在 `types` 里的事件；队列里已有则立即返回。 */
  next(types: string[], timeoutMs: number): Promise<T> {
    const idx = this.queue.findIndex((q) => types.includes(q.t))
    if (idx >= 0) return Promise.resolve(this.queue.splice(idx, 1)[0]!)
    if (this.closed) return Promise.reject(new MailboxClosed('closed'))
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiter = null
        reject(new MailboxTimeout('timeout'))
      }, timeoutMs)
      this.waiter = { types, resolve, reject, timer }
    })
  }
}
