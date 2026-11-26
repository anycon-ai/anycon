// 派生 Worker（见 kdf.ts）：自己加载一份 wasm，做一次 Argon2id，把结果转移回页面。
// 页面拿到回复就 terminate 它，这份 wasm 实例和它撑大的 64 MiB 内存随之释放。
import { loadCrypto, type FullCrypto } from './crypto'
import type { KdfReply, KdfRequest } from './kdf'

interface WorkerScope {
  onmessage: ((e: MessageEvent<KdfRequest>) => void) | null
  postMessage(msg: KdfReply, transfer?: Transferable[]): void
}

const scope = self as unknown as WorkerScope

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

scope.onmessage = (e) => {
  void handle(e.data)
}

async function handle(req: KdfRequest): Promise<void> {
  let crypto: FullCrypto
  try {
    crypto = await loadCrypto()
  } catch (e) {
    scope.postMessage({ ok: false, stage: 'load', message: message(e) })
    return
  }
  try {
    if (req.op === 'pair') {
      const s = crypto.derivePairSecrets(req.code)
      // 转移而不是拷贝：这边不留副本
      scope.postMessage({ ok: true, op: 'pair', matchId: s.matchId, psk: s.psk }, [s.psk.buffer as ArrayBuffer])
    } else {
      const prs = crypto.deriveEnrollPasswordPrs(req.password, req.salt)
      scope.postMessage({ ok: true, op: 'enroll_password', prs }, [prs.buffer as ArrayBuffer])
    }
  } catch (e) {
    scope.postMessage({ ok: false, stage: 'derive', message: message(e) })
  }
}
