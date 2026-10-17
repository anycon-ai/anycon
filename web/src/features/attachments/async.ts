// 附件上传 / 下载共用的小工具：并发限制、重试、让出主线程。
import { RpcCallError } from '@/transport/rpcClient'

export class AbortedError extends Error {
  constructor() {
    super('aborted')
    this.name = 'AbortedError'
  }
}

export const isAborted = (e: unknown): boolean => e instanceof AbortedError

/** 让出主线程一拍（编码 base64、发请求之间，别把界面卡住）。 */
export function yieldToMain(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0))
}

export const sleep = (ms: number): Promise<void> => (ms > 0 ? new Promise((resolve) => setTimeout(resolve, ms)) : yieldToMain())

/** 先进先出的并发限制：同时最多 `max` 个任务在跑，其余排队。 */
export interface Limiter {
  run<T>(task: () => Promise<T>): Promise<T>
  /**
   * 排队等一个名额；拿到时返回「归还」函数（可以多次调用，只第一次有效）。
   * 用在名额要跨过几个步骤、中途才交出去的地方（先规整、拿到上传名额之后才放掉规整名额）。
   * `signal` 中止时还在排队的就退出队列并以 `AbortedError` 拒绝（已经拿到的不受影响，由调用方归还）。
   */
  acquire(signal?: AbortSignal): Promise<() => void>
  /** 正在占用的名额数（测试用）。 */
  readonly active: number
}

export function createLimiter(max: number): Limiter {
  let active = 0
  const queue: Array<() => void> = []
  /** 有名额就立刻（同步）交出去，没有就排队；归还时直接把名额交给队首。返回「退出队列」：还没轮到时才有效，返回是否退成了。 */
  const enter = (granted: (release: () => void) => void): (() => boolean) => {
    const give = (): void => {
      let released = false
      granted(() => {
        if (released) return
        released = true
        const next = queue.shift()
        if (next) next()
        else active -= 1
      })
    }
    if (active < max) {
      active += 1
      give()
      return () => false
    }
    queue.push(give)
    return () => {
      const i = queue.indexOf(give)
      if (i < 0) return false
      queue.splice(i, 1)
      return true
    }
  }
  return {
    get active() {
      return active
    },
    run<T>(task: () => Promise<T>): Promise<T> {
      return new Promise<T>((resolve, reject) => {
        enter((release) => {
          let p: Promise<T>
          try {
            p = task()
          } catch (e) {
            p = Promise.reject(e instanceof Error ? e : new Error(String(e)))
          }
          p.then(resolve, reject).finally(release)
        })
      })
    },
    acquire(signal?: AbortSignal): Promise<() => void> {
      return new Promise((resolve, reject) => {
        if (signal?.aborted) return reject(new AbortedError())
        let settled = false
        let cancel: (() => boolean) | null = null
        const onAbort = (): void => {
          if (cancel?.()) reject(new AbortedError())
        }
        cancel = enter((release) => {
          settled = true
          signal?.removeEventListener('abort', onAbort)
          resolve(release)
        })
        if (!settled) signal?.addEventListener('abort', onAbort, { once: true })
      })
    },
  }
}

/**
 * 值得重试的失败：链路层的（超时、节点离线、undeliverable、连接断开），以及 daemon 说「忙 / 内部错误」。
 * daemon 明确拒绝的（`invalid`、`not_found`、`unsupported` …）重试也是同样的结果。
 */
export function isTransient(e: unknown): boolean {
  if (!(e instanceof RpcCallError)) return false
  if (e.kind === 'too_large') return false
  if (e.kind !== 'rpc') return true
  return e.code === 'busy' || e.code === 'node_busy' || e.code === 'internal'
}

/** 失败后最多再试 `retries` 次（共 `retries + 1` 次），每次之间等 `delayMs × 第几次重试`。中止（`signal`）立刻停。 */
export async function withRetry<T>(fn: () => Promise<T>, opts: { retries: number; delayMs: number; signal?: AbortSignal }): Promise<T> {
  for (let attempt = 0; ; attempt++) {
    if (opts.signal?.aborted) throw new AbortedError()
    try {
      return await fn()
    } catch (e) {
      if (attempt >= opts.retries || !isTransient(e)) throw e
      await sleep(opts.delayMs * (attempt + 1))
    }
  }
}
