// 两处 Argon2id（配对短码 → match_id + PSK；节点口令 → prs）放到一次性的 Web Worker 里算。
//
// Argon2id 要 64 MiB 内存，wasm 的线性内存只增不减：在页面自己的 wasm 实例里算过一次，这 64 MiB 就一直挂在
// 页面上直到刷新。Worker 有自己的 wasm 实例，算完 `terminate()`，内存随 Worker 一起释放；顺带不再卡主线程。
// 主线程的 wasm 实例因此不能再碰这两个函数。
//
// Worker 起不来（脚本加载失败、wasm 加载失败）时退回在主线程上同步算：配对 / 接入不能因此失败，只是那 64 MiB 又会留在页面上。
// 派生本身失败（比如分配不出内存）照常抛错，不退回。
import type { FullCrypto } from './crypto'

/** 配对短码派生出的两样东西。`psk` 用完 `fill(0)`。 */
export interface PairSecrets {
  matchId: string
  psk: Uint8Array
}

/** 要 64 MiB 内存的那两个派生。 */
export interface Kdf {
  derivePairSecrets(code: string): Promise<PairSecrets>
  deriveEnrollPasswordPrs(password: string, salt: Uint8Array): Promise<Uint8Array>
}

/** 发给 Worker 的请求。 */
export type KdfRequest = { op: 'pair'; code: string } | { op: 'enroll_password'; password: string; salt: Uint8Array }

/** Worker 的回复。`stage: 'load'` 是 wasm 没加载起来（可以退回主线程），`'derive'` 是派生本身失败。 */
export type KdfReply =
  | { ok: true; op: 'pair'; matchId: string; psk: Uint8Array }
  | { ok: true; op: 'enroll_password'; prs: Uint8Array }
  | { ok: false; stage: 'load' | 'derive'; message: string }

type KdfResult = Extract<KdfReply, { ok: true }>

/** `Worker` 里用到的部分（测试里换成假的）。 */
export interface WorkerLike {
  onmessage: ((e: MessageEvent<KdfReply>) => void) | null
  onerror: ((e: ErrorEvent) => void) | null
  onmessageerror: ((e: MessageEvent) => void) | null
  postMessage(msg: KdfRequest): void
  terminate(): void
}

/** 一次派生最多等这么久（浏览器里通常 0.1–1 s，很慢的手机也就几秒）。 */
export const KDF_TIMEOUT_MS = 60_000

/** 直接在给定的 crypto 上同步算：单元测试、没有 Worker 的环境，以及 Worker 起不来时的退路。 */
export function inlineKdf(crypto: FullCrypto): Kdf {
  return {
    derivePairSecrets: (code) => Promise.resolve().then(() => crypto.derivePairSecrets(code)),
    deriveEnrollPasswordPrs: (password, salt) => Promise.resolve().then(() => crypto.deriveEnrollPasswordPrs(password, salt)),
  }
}

/** 起一个新的派生 Worker。写法必须是字面量的 `new Worker(new URL(…), …)`，Vite 才会把它单独打包。 */
export function spawnKdfWorker(): WorkerLike {
  return new Worker(new URL('./kdf.worker.ts', import.meta.url), { type: 'module', name: 'anycon-kdf' }) as unknown as WorkerLike
}

/** Worker 没能给出结果，可以退回主线程再算一次。 */
class WorkerUnavailable extends Error {}

/** 一次派生：新开一个 Worker，拿到结果（或失败）就 `terminate()`。 */
function runOnce(spawn: () => WorkerLike, req: KdfRequest, timeoutMs: number): Promise<KdfResult> {
  return new Promise((resolve, reject) => {
    let w: WorkerLike
    try {
      w = spawn()
    } catch (e) {
      reject(new WorkerUnavailable(`could not start the key derivation worker: ${(e as Error).message}`))
      return
    }
    const timer = setTimeout(() => finish(() => reject(new Error('key derivation timed out'))), timeoutMs)
    function finish(settle: () => void): void {
      clearTimeout(timer)
      w.onmessage = null
      w.onerror = null
      w.onmessageerror = null
      w.terminate()
      settle()
    }
    w.onmessage = (e) => {
      const r = e.data
      finish(() => {
        if (!r.ok) reject(r.stage === 'load' ? new WorkerUnavailable(r.message) : new Error(r.message))
        else if (r.op !== req.op) reject(new Error(`the key derivation worker answered ${r.op} to ${req.op}`))
        else resolve(r)
      })
    }
    w.onerror = (e) => {
      // 脚本没加载起来（网络、CSP）或者 Worker 里抛了没接住的错
      e.preventDefault()
      finish(() => reject(new WorkerUnavailable(e.message || 'the key derivation worker failed')))
    }
    w.onmessageerror = () => finish(() => reject(new WorkerUnavailable('could not read the key derivation worker reply')))
    w.postMessage(req)
  })
}

/**
 * 每次派生都新开一个 Worker、算完就关掉的 `Kdf`。Worker 不可用时用 `fallback`（主线程同步算）。
 * `spawn` / `timeoutMs` 给测试换。
 */
export function workerKdf(fallback: Kdf, spawn: () => WorkerLike = spawnKdfWorker, timeoutMs = KDF_TIMEOUT_MS): Kdf {
  async function run<T>(req: KdfRequest, viaWorker: (r: KdfResult) => T, inline: () => Promise<T>): Promise<T> {
    try {
      return viaWorker(await runOnce(spawn, req, timeoutMs))
    } catch (e) {
      if (!(e instanceof WorkerUnavailable)) throw e
      console.warn('anycon: the key derivation worker is unavailable; deriving on the main thread instead', e)
      return inline()
    }
  }
  return {
    derivePairSecrets: (code) =>
      run(
        { op: 'pair', code },
        (r) => {
          if (r.op !== 'pair') throw new Error('unexpected key derivation reply')
          return { matchId: r.matchId, psk: r.psk }
        },
        () => fallback.derivePairSecrets(code),
      ),
    deriveEnrollPasswordPrs: (password, salt) =>
      run(
        { op: 'enroll_password', password, salt },
        (r) => {
          if (r.op !== 'enroll_password') throw new Error('unexpected key derivation reply')
          return r.prs
        },
        () => fallback.deriveEnrollPasswordPrs(password, salt),
      ),
  }
}
