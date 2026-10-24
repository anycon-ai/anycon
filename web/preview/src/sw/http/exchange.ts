// 一次 HTTP 往返（一个隧道 stream）的页面一侧接口：响应头是一个 Promise，响应体是 `ReadableStream`。
//
// 响应体按**拉取**驱动：流的 `highWaterMark` 是 0，页面（浏览器）读一块我们才从队列里取一块，同时把读走的字节数还给
// 对端（`TunnelStream.consumed` → `WINDOW`）。所以对端发得多快取决于页面读得多快，而不是我们能缓冲多少
// （docs/protocol.md § 11.5、spec/web.md § 13.2）。
import type { PreviewRequestHead, PreviewResetCode, PreviewResponseHead } from '@anycon/protocol'
import type { StreamEnd, StreamListener, TunnelSession, TunnelStream } from '../tunnel/session'

/** 等响应头最多多久。daemon 等上游最多 120 秒，这里多留几秒让它的 `RESET { timeout }` 先到。 */
export const HEAD_TIMEOUT_MS = 125_000

export type ExchangeFailure =
  | { kind: 'reset'; code: PreviewResetCode; message?: string }
  | { kind: 'transport'; message: string }
  | { kind: 'timeout' }
  | { kind: 'aborted' }

export class ExchangeError extends Error {
  override readonly name = 'ExchangeError'
  constructor(readonly failure: ExchangeFailure) {
    super(failure.kind === 'reset' ? `reset: ${failure.code}` : failure.kind === 'transport' ? failure.message : failure.kind)
  }
}

export interface HttpExchange {
  readonly head: Promise<PreviewResponseHead>
  readonly body: ReadableStream<Uint8Array>
  /** 不要了：`RESET { cancelled }`，响应体流报错。 */
  abort(): void
}

/** 收到的响应体块的队列，读的一方等数据到来。 */
class ChunkQueue {
  private readonly chunks: Uint8Array[] = []
  private done = false
  private error: Error | undefined
  private wake: (() => void) | undefined

  push(chunk: Uint8Array): void {
    this.chunks.push(chunk)
    this.notify()
  }
  end(): void {
    this.done = true
    this.notify()
  }
  fail(err: Error): void {
    this.error ??= err
    this.notify()
  }
  private notify(): void {
    const w = this.wake
    this.wake = undefined
    w?.()
  }

  async read(): Promise<Uint8Array | null> {
    for (;;) {
      if (this.error) throw this.error
      const chunk = this.chunks.shift()
      if (chunk) return chunk
      if (this.done) return null
      await new Promise<void>((resolve) => (this.wake = resolve))
    }
  }
}

function failureOf(end: StreamEnd): ExchangeFailure {
  return end.kind === 'reset' ? { kind: 'reset', code: end.code, message: end.message } : { kind: 'transport', message: end.message }
}

export async function openHttp(
  session: TunnelSession,
  request: PreviewRequestHead,
  body?: Uint8Array,
  headTimeoutMs: number = HEAD_TIMEOUT_MS,
): Promise<HttpExchange> {
  const queue = new ChunkQueue()
  let resolveHead!: (h: PreviewResponseHead) => void
  let rejectHead!: (e: Error) => void
  const head = new Promise<PreviewResponseHead>((resolve, reject) => {
    resolveHead = resolve
    rejectHead = reject
  })
  head.catch(() => {}) // 没人等响应头就被中止时不算未处理的拒绝

  let stream: TunnelStream | undefined
  let finished = false
  const fail = (failure: ExchangeFailure): void => {
    if (finished) return
    finished = true
    clearTimeout(timer)
    const err = new ExchangeError(failure)
    rejectHead(err)
    queue.fail(err)
  }
  const timer = setTimeout(() => {
    fail({ kind: 'timeout' })
    stream?.abort('cancelled')
  }, headTimeoutMs)

  const listener: StreamListener = {
    response(h) {
      clearTimeout(timer)
      resolveHead(h)
    },
    data: (c) => queue.push(c),
    end: () => {
      finished = true
      queue.end()
    },
    failed: (end) => fail(failureOf(end)),
  }
  try {
    stream = await session.openStream(request, listener, body)
  } catch (err) {
    clearTimeout(timer)
    throw err
  }
  const s = stream
  const readable = new ReadableStream<Uint8Array>(
    {
      async pull(controller) {
        const chunk = await queue.read()
        if (chunk === null) controller.close()
        else {
          s.consumed(chunk.length)
          controller.enqueue(chunk)
        }
      },
      cancel() {
        clearTimeout(timer)
        if (!finished) {
          finished = true
          s.abort('cancelled')
        }
      },
    },
    { highWaterMark: 0 },
  )
  return {
    head,
    body: readable,
    abort() {
      clearTimeout(timer)
      if (finished) return
      fail({ kind: 'aborted' })
      s.abort('cancelled')
    },
  }
}
