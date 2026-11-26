// 一次性派生 Worker 的编排：每次新开、拿到结果或失败都关掉；Worker 不可用时退回主线程，派生本身失败不退回。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createFakeCrypto } from '@/test/fakeCrypto'
import { inlineKdf, workerKdf, type KdfReply, type KdfRequest, type WorkerLike } from './kdf'

class FakeWorker implements WorkerLike {
  static all: FakeWorker[] = []
  onmessage: ((e: MessageEvent<KdfReply>) => void) | null = null
  onerror: ((e: ErrorEvent) => void) | null = null
  onmessageerror: ((e: MessageEvent) => void) | null = null
  requests: KdfRequest[] = []
  terminated = false
  constructor() {
    FakeWorker.all.push(this)
  }
  postMessage(msg: KdfRequest): void {
    this.requests.push(msg)
  }
  terminate(): void {
    this.terminated = true
  }
  reply(r: KdfReply): void {
    this.onmessage?.(new MessageEvent('message', { data: r }))
  }
  fail(message: string): void {
    const e = new ErrorEvent('error', { message, cancelable: true })
    this.onerror?.(e)
  }
}

const spawn = (): WorkerLike => new FakeWorker()
const last = (): FakeWorker => FakeWorker.all.at(-1)!

function fallbackSpy() {
  const inline = inlineKdf(createFakeCrypto())
  return {
    kdf: inline,
    pair: vi.spyOn(inline, 'derivePairSecrets'),
    password: vi.spyOn(inline, 'deriveEnrollPasswordPrs'),
  }
}

beforeEach(() => {
  FakeWorker.all = []
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => {
  vi.useRealTimers()
  vi.restoreAllMocks()
})

describe('workerKdf', () => {
  it('derives the pairing secrets in a fresh worker, terminates it, and never touches the main-thread crypto', async () => {
    const fb = fallbackSpy()
    const kdf = workerKdf(fb.kdf, spawn)
    const p = kdf.derivePairSecrets('ABCD2345')
    expect(last().requests).toEqual([{ op: 'pair', code: 'ABCD2345' }])
    const psk = new Uint8Array(32).fill(7)
    last().reply({ ok: true, op: 'pair', matchId: '0011223344556677', psk })
    await expect(p).resolves.toEqual({ matchId: '0011223344556677', psk })
    expect(last().terminated).toBe(true)
    expect(fb.pair).not.toHaveBeenCalled()

    // 下一次又是一个新的 Worker
    const q = kdf.deriveEnrollPasswordPrs('correct horse', Uint8Array.of(1, 2, 3))
    expect(FakeWorker.all).toHaveLength(2)
    expect(last().requests).toEqual([{ op: 'enroll_password', password: 'correct horse', salt: Uint8Array.of(1, 2, 3) }])
    last().reply({ ok: true, op: 'enroll_password', prs: Uint8Array.of(9, 9) })
    await expect(q).resolves.toEqual(Uint8Array.of(9, 9))
    expect(last().terminated).toBe(true)
    expect(fb.password).not.toHaveBeenCalled()
  })

  it('a failing derivation is an error, not a reason to try again on the main thread', async () => {
    const fb = fallbackSpy()
    const p = workerKdf(fb.kdf, spawn).derivePairSecrets('ABCD2345')
    last().reply({ ok: false, stage: 'derive', message: 'out of memory' })
    await expect(p).rejects.toThrow('out of memory')
    expect(last().terminated).toBe(true)
    expect(fb.pair).not.toHaveBeenCalled()
  })

  it('falls back to the main thread when the worker cannot load the wasm', async () => {
    const fb = fallbackSpy()
    const p = workerKdf(fb.kdf, spawn).derivePairSecrets('ABCD2345')
    last().reply({ ok: false, stage: 'load', message: 'anycon crypto wasm is not built' })
    const s = await p
    expect(s.psk).toHaveLength(32)
    expect(fb.pair).toHaveBeenCalledWith('ABCD2345')
    expect(last().terminated).toBe(true)
    expect(console.warn).toHaveBeenCalled()
  })

  it('falls back to the main thread when the worker script fails or cannot be started', async () => {
    const fb = fallbackSpy()
    const p = workerKdf(fb.kdf, spawn).deriveEnrollPasswordPrs('correct horse', new Uint8Array(16))
    last().fail('blocked by CSP')
    await expect(p).resolves.toBeInstanceOf(Uint8Array)
    expect(fb.password).toHaveBeenCalledTimes(1)
    expect(last().terminated).toBe(true)

    const broken = workerKdf(fb.kdf, () => {
      throw new Error('Worker is not defined')
    })
    await expect(broken.derivePairSecrets('ABCD2345')).resolves.toMatchObject({ psk: expect.any(Uint8Array) })
    expect(fb.pair).toHaveBeenCalledTimes(1)
  })

  it('gives up and terminates the worker when it never answers', async () => {
    vi.useFakeTimers()
    const fb = fallbackSpy()
    const p = workerKdf(fb.kdf, spawn, 5_000).derivePairSecrets('ABCD2345')
    const settled = expect(p).rejects.toThrow('timed out')
    await vi.advanceTimersByTimeAsync(5_000)
    await settled
    expect(last().terminated).toBe(true)
    expect(fb.pair).not.toHaveBeenCalled()
  })

  it('rejects a reply for the other operation', async () => {
    const p = workerKdf(fallbackSpy().kdf, spawn).derivePairSecrets('ABCD2345')
    last().reply({ ok: true, op: 'enroll_password', prs: new Uint8Array(32) })
    await expect(p).rejects.toThrow(/answered enroll_password to pair/)
    expect(last().terminated).toBe(true)
  })
})
