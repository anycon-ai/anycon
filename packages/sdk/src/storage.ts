/**
 * 存储抽象。SDK 是一台设备：本地要持久化设备身份（Noise 静态私钥 + hub 签发的 device token）
 * 和已配对节点（daemon 的静态公钥）。后端只需要实现最简单的键值接口：
 *
 * - `MemoryStorage`：进程内，测试 / 一次性脚本；
 * - `FileStorage(dir)`：Node，`~/.anycon/sdk/<hub-host>/<profile>/`，文件 0600（见 storage-file.ts）；
 * - `IndexedDBStorage`：浏览器（见 storage-idb.ts）。
 *
 * CI 里可用环境变量注入、不落盘：`ANYCON_DEVICE_KEY`（32 字节私钥 hex）、`ANYCON_DEVICE_TOKEN`，
 * 以及要连的节点 `ANYCON_NODE_ID` + `ANYCON_NODE_KEY`（daemon 公钥 hex）。
 */
import { AnyconError, NodeKeyChanged } from './errors.js'

/** 最简键值存储。值是字符串（JSON）。 */
export interface SdkStorage {
  get(key: string): Promise<string | undefined>
  set(key: string, value: string): Promise<void>
  delete(key: string): Promise<void>
}

export class MemoryStorage implements SdkStorage {
  readonly #map = new Map<string, string>()

  get(key: string): Promise<string | undefined> {
    return Promise.resolve(this.#map.get(key))
  }
  set(key: string, value: string): Promise<void> {
    this.#map.set(key, value)
    return Promise.resolve()
  }
  delete(key: string): Promise<void> {
    this.#map.delete(key)
    return Promise.resolve()
  }
}

/** 本设备在某个 hub 上的身份。 */
export interface StoredIdentity {
  /** hub 分配的设备 id（32 hex）。 */
  deviceId: string
  /** hub 签发的设备 token。敏感。 */
  deviceToken: string
  /** Noise 静态私钥，hex。敏感。 */
  secretKey: string
  /** 配对时给 hub 看的显示名。 */
  name: string
  createdAtMs: number
}

/** 已配对的节点（daemon）。 */
export interface StoredNode {
  /** 节点（daemon）的设备 id。 */
  id: string
  /** 显示名；配对时不知道，首次 `node.info` 后补上。 */
  name?: string
  /** daemon 的 Noise 静态公钥，hex。 */
  publicKey: string
  fingerprint?: string
  pairedAtMs: number
}

const IDENTITY_KEY = 'identity'
const NODES_KEY = 'nodes'

/** 在 `SdkStorage` 之上的有类型封装；同进程内的读改写串行化。 */
export class StateStore {
  readonly storage: SdkStorage
  #chain: Promise<unknown> = Promise.resolve()

  constructor(storage: SdkStorage) {
    this.storage = storage
  }

  async getIdentity(): Promise<StoredIdentity | undefined> {
    const raw = await this.storage.get(IDENTITY_KEY)
    return raw === undefined ? undefined : parse<StoredIdentity>(IDENTITY_KEY, raw)
  }

  setIdentity(identity: StoredIdentity): Promise<void> {
    return this.storage.set(IDENTITY_KEY, JSON.stringify(identity))
  }

  /** 设备被吊销后清掉本地凭据（私钥 + token）；节点公钥记录留着，重新配对会覆盖。 */
  clearIdentity(): Promise<void> {
    return this.storage.delete(IDENTITY_KEY)
  }

  async getNodes(): Promise<StoredNode[]> {
    const raw = await this.storage.get(NODES_KEY)
    return raw === undefined ? [] : parse<StoredNode[]>(NODES_KEY, raw)
  }

  /**
   * 配对要落盘节点之前先查：本地已有同 id 节点且公钥不同 → `NodeKeyChanged`（不静默覆盖已固定的公钥）。
   * 同 id 同公钥（比如吊销后重新配对）是允许的。
   */
  checkNodeKey(node: Pick<StoredNode, 'id' | 'publicKey' | 'fingerprint'>): Promise<void> {
    return this.#serial(async () => {
      const known = (await this.getNodes()).find((n) => n.id === node.id)
      if (known && known.publicKey.toLowerCase() !== node.publicKey.toLowerCase()) {
        throw new NodeKeyChanged(node.id, known.fingerprint, node.fingerprint)
      }
    })
  }

  /** 配对落盘节点：检查公钥（同 `checkNodeKey`）和写入在同一个串行区里完成。 */
  pinNode(node: StoredNode): Promise<void> {
    return this.#serial(async () => {
      const nodes = await this.getNodes()
      const known = nodes.find((n) => n.id === node.id)
      if (known && known.publicKey.toLowerCase() !== node.publicKey.toLowerCase()) {
        throw new NodeKeyChanged(node.id, known.fingerprint, node.fingerprint)
      }
      await this.storage.set(
        NODES_KEY,
        JSON.stringify([...nodes.filter((n) => n.id !== node.id), node]),
      )
    })
  }

  /** 新增或按 id 覆盖，不比较公钥（测试 / 迁移用；配对流程用 `pinNode`）。 */
  putNode(node: StoredNode): Promise<void> {
    return this.#serial(async () => {
      const nodes = (await this.getNodes()).filter((n) => n.id !== node.id)
      nodes.push(node)
      await this.storage.set(NODES_KEY, JSON.stringify(nodes))
    })
  }

  /** 合并更新已有节点（比如补上名字）；节点不存在则忽略。 */
  updateNode(id: string, patch: Partial<StoredNode>): Promise<void> {
    return this.#serial(async () => {
      const nodes = await this.getNodes()
      const idx = nodes.findIndex((n) => n.id === id)
      if (idx < 0) return
      nodes[idx] = { ...nodes[idx]!, ...patch, id }
      await this.storage.set(NODES_KEY, JSON.stringify(nodes))
    })
  }

  removeNode(id: string): Promise<void> {
    return this.#serial(async () => {
      const nodes = (await this.getNodes()).filter((n) => n.id !== id)
      await this.storage.set(NODES_KEY, JSON.stringify(nodes))
    })
  }

  #serial<T>(fn: () => Promise<T>): Promise<T> {
    const run = this.#chain.then(fn, fn)
    this.#chain = run.catch(() => undefined)
    return run
  }
}

function parse<T>(key: string, raw: string): T {
  try {
    return JSON.parse(raw) as T
  } catch (cause) {
    throw new AnyconError(
      'storage_corrupt',
      `stored "${key}" is not valid JSON; remove it and pair again`,
      { cause },
    )
  }
}

type Env = Record<string, string | undefined>

function processEnv(): Env {
  const p = (globalThis as { process?: { env?: Env } }).process
  return p?.env ?? {}
}

/** 读环境变量里注入的凭据（CI 用）。身份需 key + token 同时给；节点需 id + key 同时给。 */
export function envCredentials(env: Env = processEnv()): {
  identity?: Pick<StoredIdentity, 'secretKey' | 'deviceToken'> & { deviceId?: string }
  node?: StoredNode
} {
  const out: ReturnType<typeof envCredentials> = {}
  const key = env.ANYCON_DEVICE_KEY
  const token = env.ANYCON_DEVICE_TOKEN
  if (key && token) {
    out.identity = { secretKey: key, deviceToken: token }
    if (env.ANYCON_DEVICE_ID) out.identity.deviceId = env.ANYCON_DEVICE_ID
  }
  if (env.ANYCON_NODE_ID && env.ANYCON_NODE_KEY) {
    out.node = {
      id: env.ANYCON_NODE_ID,
      publicKey: env.ANYCON_NODE_KEY,
      pairedAtMs: 0,
      ...(env.ANYCON_NODE_NAME ? { name: env.ANYCON_NODE_NAME } : {}),
    }
  }
  return out
}

/** `https://hub.example.com:8443/x` → `hub.example.com_8443`，用作目录 / 数据库名。 */
export function hubHostKey(hubUrl: string): string {
  const url = new URL(hubUrl.includes('://') ? hubUrl : `https://${hubUrl}`)
  const key = url.host.replace(/[^A-Za-z0-9.-]/g, '_')
  // 目录名不能是 `.` / `..`（也不能为空），否则会跑出 SDK 的存储根。
  if (key === '' || /^\.+$/.test(key)) {
    throw new AnyconError(
      'invalid_hub_url',
      `cannot derive a storage name from hub URL "${hubUrl}"`,
    )
  }
  return key
}

const PROFILE_RE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/

/**
 * profile 会变成存储目录的一级（`~/.anycon/sdk/<hub-host>/<profile>/`）：只允许字母数字和 `. _ -`，
 * 以字母数字开头、不超过 64 位、不含 `..`。路径分隔符、`..`、空串一律拒绝（抛 `AnyconError('invalid_profile')`），
 * 而不是悄悄改写——否则 `a/b` 和 `a_b` 会撞到同一个目录。
 */
export function assertSafeProfile(profile: string): string {
  if (!PROFILE_RE.test(profile) || profile.includes('..')) {
    throw new AnyconError(
      'invalid_profile',
      `invalid profile ${JSON.stringify(profile)}: use letters, digits, ".", "_" or "-" (at most 64, starting with a letter or digit; no path separators or "..")`,
    )
  }
  return profile
}

/** 当前 profile：显式参数 > `ANYCON_PROFILE`（空串当作没设）> `default`。不合法抛 `invalid_profile`。 */
export function resolveProfile(profile?: string, env: Env = processEnv()): string {
  return assertSafeProfile(profile ?? (env.ANYCON_PROFILE || 'default'))
}
