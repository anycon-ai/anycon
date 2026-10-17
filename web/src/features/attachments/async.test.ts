import { describe, expect, it, vi } from 'vitest'
import { rpcError } from '@/test/fakeRpc'
import { RpcCallError } from '@/transport/rpcClient'
import { AbortedError, createLimiter, isTransient, withRetry } from './async'

describe('createLimiter', () => {
  it('runs at most `max` tasks at once and starts queued ones in order as slots free up', async () => {
    const limiter = createLimiter(2)
    const started: number[] = []
    let peak = 0
    const gates: Array<() => void> = []
    const task = (n: number) => () =>
      new Promise<number>((resolve) => {
        started.push(n)
        peak = Math.max(peak, limiter.active)
        gates.push(() => resolve(n))
      })
    const results = [1, 2, 3, 4].map((n) => limiter.run(task(n)))
    expect(started).toEqual([1, 2])
    gates[0]!()
    await vi.waitFor(() => expect(started).toEqual([1, 2, 3]))
    gates[1]!()
    await vi.waitFor(() => expect(started).toEqual([1, 2, 3, 4]))
    gates[2]!()
    gates[3]!()
    expect(await Promise.all(results)).toEqual([1, 2, 3, 4])
    expect(peak).toBe(2)
    expect(limiter.active).toBe(0)
  })

  it('a failing task frees its slot and rejects only its own promise', async () => {
    const limiter = createLimiter(1)
    const bad = limiter.run(() => Promise.reject(new Error('boom')))
    const good = limiter.run(async () => 'ok')
    await expect(bad).rejects.toThrow('boom')
    await expect(good).resolves.toBe('ok')
  })

  it('a task that throws synchronously is a rejection, not a stuck slot', async () => {
    const limiter = createLimiter(1)
    await expect(
      limiter.run(() => {
        throw new Error('sync')
      }),
    ).rejects.toThrow('sync')
    await expect(limiter.run(async () => 1)).resolves.toBe(1)
  })
})

describe('createLimiter.acquire', () => {
  it('hands out `max` slots, queues the rest in order, and a release passes its slot straight to the next in line', async () => {
    const limiter = createLimiter(1)
    const order: number[] = []
    const first = await limiter.acquire()
    expect(limiter.active).toBe(1)
    const second = limiter.acquire().then((release) => (order.push(2), release))
    const third = limiter.acquire().then((release) => (order.push(3), release))
    await Promise.resolve()
    expect(order).toEqual([])
    first()
    first() // 重复归还没有副作用
    const r2 = await second
    expect(order).toEqual([2])
    expect(limiter.active).toBe(1)
    r2()
    ;(await third)()
    expect(order).toEqual([2, 3])
    expect(limiter.active).toBe(0)
  })

  it('shares the slots with run()', async () => {
    const limiter = createLimiter(1)
    const release = await limiter.acquire()
    let ran = false
    const task = limiter.run(async () => (ran = true))
    await Promise.resolve()
    expect(ran).toBe(false)
    release()
    await task
    expect(ran).toBe(true)
  })

  it('an aborted waiter leaves the queue (rejects with AbortedError) and the others keep their order; an already aborted signal never queues', async () => {
    const limiter = createLimiter(1)
    const held = await limiter.acquire()
    const ctl = new AbortController()
    const waiting = limiter.acquire(ctl.signal)
    const behind = limiter.acquire()
    ctl.abort()
    await expect(waiting).rejects.toBeInstanceOf(AbortedError)
    held()
    const release = await behind // 轮到的是排在被中止的那个后面的
    expect(limiter.active).toBe(1)
    release()
    expect(limiter.active).toBe(0)

    const done = new AbortController()
    done.abort()
    await expect(limiter.acquire(done.signal)).rejects.toBeInstanceOf(AbortedError)
    expect(limiter.active).toBe(0)
  })

  it('aborting after the slot was granted does nothing (the holder gives it back itself)', async () => {
    const limiter = createLimiter(1)
    const ctl = new AbortController()
    const release = await limiter.acquire(ctl.signal)
    ctl.abort()
    expect(limiter.active).toBe(1)
    release()
    expect(limiter.active).toBe(0)
  })
})

describe('isTransient', () => {
  it('link-level failures and a busy / failing node are worth retrying; definite refusals are not', () => {
    for (const kind of ['timeout', 'offline', 'undeliverable', 'closed'] as const) expect(isTransient(new RpcCallError(kind, kind))).toBe(true)
    expect(isTransient(rpcError('busy'))).toBe(true)
    expect(isTransient(rpcError('node_busy'))).toBe(true)
    expect(isTransient(rpcError('internal'))).toBe(true)
    for (const code of ['invalid', 'not_found', 'unsupported', 'forbidden'] as const) expect(isTransient(rpcError(code))).toBe(false)
    expect(isTransient(new RpcCallError('too_large', 'x'))).toBe(false)
    expect(isTransient(new Error('plain'))).toBe(false)
  })
})

describe('withRetry', () => {
  it('retries transient failures up to the limit, then gives up with the last error', async () => {
    let n = 0
    const fn = vi.fn(async () => {
      n += 1
      throw new RpcCallError('timeout', `try ${n}`)
    })
    await expect(withRetry(fn, { retries: 2, delayMs: 0 })).rejects.toThrow('try 3')
    expect(fn).toHaveBeenCalledTimes(3)
  })

  it('stops at the first success', async () => {
    let n = 0
    const fn = vi.fn(async () => {
      n += 1
      if (n < 2) throw new RpcCallError('offline', 'x')
      return 'done'
    })
    await expect(withRetry(fn, { retries: 2, delayMs: 0 })).resolves.toBe('done')
    expect(fn).toHaveBeenCalledTimes(2)
  })

  it('does not retry a definite refusal', async () => {
    const fn = vi.fn(async () => {
      throw rpcError('invalid', undefined, 'incomplete')
    })
    await expect(withRetry(fn, { retries: 2, delayMs: 0 })).rejects.toMatchObject({ code: 'invalid' })
    expect(fn).toHaveBeenCalledTimes(1)
  })

  it('does not start (or retry) once aborted', async () => {
    const ctl = new AbortController()
    ctl.abort()
    const fn = vi.fn(async () => 1)
    await expect(withRetry(fn, { retries: 2, delayMs: 0, signal: ctl.signal })).rejects.toBeInstanceOf(AbortedError)
    expect(fn).not.toHaveBeenCalled()
  })

  it('waits a little longer before each further retry', async () => {
    vi.useFakeTimers()
    try {
      const fn = vi.fn(async () => {
        throw new RpcCallError('timeout', 'x')
      })
      const p = withRetry(fn, { retries: 2, delayMs: 100 }).catch(() => {})
      await vi.advanceTimersByTimeAsync(0)
      expect(fn).toHaveBeenCalledTimes(1)
      await vi.advanceTimersByTimeAsync(99)
      expect(fn).toHaveBeenCalledTimes(1)
      await vi.advanceTimersByTimeAsync(1)
      expect(fn).toHaveBeenCalledTimes(2)
      await vi.advanceTimersByTimeAsync(199)
      expect(fn).toHaveBeenCalledTimes(2)
      await vi.advanceTimersByTimeAsync(1)
      expect(fn).toHaveBeenCalledTimes(3)
      await p
    } finally {
      vi.useRealTimers()
    }
  })
})
