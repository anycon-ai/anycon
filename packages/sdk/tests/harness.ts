import { existsSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { loadCrypto, type Crypto } from '@anycon/protocol'

import { MemoryStorage, pair, type PairResult } from '../src/index.js'
import { FakeDaemon, type FakeDaemonOptions } from './fake-daemon.js'
import { FakeHub } from './fake-hub.js'
import { mockCrypto } from './mock-crypto.js'

const wasmJs = fileURLToPath(new URL('../../protocol/wasm/anycon_crypto.js', import.meta.url))
export const wasmPresent = existsSync(wasmJs)

/** 每个场景都跑一遍的 Noise 实现：玩具实现永远在，wasm 产物存在时再加真实的。 */
export const cryptoImpls: Array<{ label: string; load: () => Promise<Crypto> }> = [
  { label: 'mock crypto', load: async () => mockCrypto },
]
if (wasmPresent) {
  cryptoImpls.push({ label: 'wasm crypto', load: () => loadCrypto() })
} else {
  console.warn(
    '[sdk tests] packages/protocol/wasm 不存在：只跑 mock crypto 的场景（先跑 `just gen-wasm`）。',
  )
}

/** 把重连 / 握手 / 超时都调快，测试不用等。 */
export const FAST = {
  reconnect: { initialMs: 15, maxMs: 60 },
  handshakeTimeoutMs: 400,
  handshakeAttempts: 2,
  connectTimeoutMs: 4000,
  requestTimeoutMs: 4000,
} as const

export interface Env {
  hub: FakeHub
  daemon: FakeDaemon
  crypto: Crypto
  storage: MemoryStorage
  /** hub 的 http 地址。 */
  url: string
  pair(options?: { name?: string }): Promise<PairResult>
  stop(): Promise<void>
}

export async function startEnv(
  crypto: Crypto,
  options: { startDaemon?: boolean; daemon?: Partial<FakeDaemonOptions> } = {},
): Promise<Env> {
  const hub = await FakeHub.start()
  const device = hub.registerDevice('daemon')
  const daemon = new FakeDaemon({
    hubUrl: hub.url,
    crypto,
    device,
    name: 'fake-node',
    ...options.daemon,
  })
  const storage = new MemoryStorage()
  if (options.startDaemon !== false) await daemon.start()
  const env: Env = {
    hub,
    daemon,
    crypto,
    storage,
    url: hub.url,
    async pair({ name = 'ci-runner' } = {}) {
      const code = await daemon.beginPairing()
      return pair(hub.url, { code, name, storage, crypto })
    },
    async stop() {
      daemon.stop()
      await hub.stop()
    },
  }
  return env
}

export async function until(
  cond: () => boolean | Promise<boolean>,
  timeoutMs = 3000,
  what = 'condition',
): Promise<void> {
  const start = Date.now()
  while (!(await cond())) {
    if (Date.now() - start > timeoutMs) throw new Error(`timed out waiting for ${what}`)
    await new Promise((r) => setTimeout(r, 5))
  }
}

export async function collect<T>(iterable: AsyncIterable<T>, limit = 500): Promise<T[]> {
  const out: T[] = []
  for await (const item of iterable) {
    out.push(item)
    if (out.length > limit) throw new Error('stream did not terminate')
  }
  return out
}

export const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))
