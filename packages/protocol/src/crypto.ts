/**
 * `crates/crypto` 的 wasm 封装。`loadCrypto()` 动态加载 `../wasm/anycon_crypto.js`（`just gen-wasm` 的产物，
 * 已 gitignore），初始化后返回一个 Promise 无关的同步 API：`Uint8Array` 进出，没有 wasm-bindgen 的所有权语义。
 *
 * 为什么包一层：
 * - 密钥对是纯数据（`public` / `secret`），每次握手时才在 wasm 里临时重建，不存在「传进去就被消费」的问题；
 * - 隐藏 wasm-bindgen 产物的细节（getter / 方法、`init` 签名、Node 下读 .wasm 文件）；
 * - SDK 和测试可以注入别的实现（`Crypto` 接口），比如不需要真密码学的 fake。
 *
 * 角色：配对（XXpsk3）joiner 是 initiator、host（daemon）是 responder；重连（IK）客户端是 initiator。
 */
import { PAIR_CODE_ALPHABET, PAIR_CODE_LEN, PAIR_CODE_LEN_LONG } from './constants.js'

export type Role = 'initiator' | 'responder'

/** X25519 静态密钥对。纯数据，可序列化保存（`secret` 是敏感信息，0600 / IndexedDB）。 */
export interface Keypair {
  readonly public: Uint8Array
  readonly secret: Uint8Array
}

/** `derivePairSecrets` 的结果。`psk` 是拷贝，用完 `fill(0)`。 */
export interface PairSecrets {
  /** 16 个 hex，发给 hub 撮合。 */
  readonly matchId: string
  /** 32 字节预共享密钥，不离开本机。 */
  readonly psk: Uint8Array
}

/** 握手完成后的双向加密通道。一个 Transport 只能单线程顺序使用。 */
export interface Transport {
  /** 加密任意长度明文（自动分片）。 */
  encrypt(plaintext: Uint8Array): Uint8Array
  /** 解密 `encrypt` 的输出，还原整条明文。乱序 / 篡改会抛错。 */
  decrypt(ciphertext: Uint8Array): Uint8Array
  /** 对端静态公钥。 */
  readonly remotePublic: Uint8Array
  /** 释放 wasm 内存。之后不能再用。 */
  free(): void
}

/** 进行中的握手。 */
export interface Handshake {
  /** 现在该我写消息吗。 */
  isMyTurn(): boolean
  /** 写下一条握手消息（可带 payload，通常为空）。 */
  write(payload?: Uint8Array): Uint8Array
  /** 读对方的握手消息，返回其 payload。 */
  read(message: Uint8Array): Uint8Array
  isFinished(): boolean
  /** 对方静态公钥；握手进行到能知道的时候起可用（IK responder 读完第一条后就有）。 */
  remotePublic(): Uint8Array | undefined
  /** 握手完成后转成 Transport，之后这个 Handshake 不能再用。 */
  intoTransport(): Transport
  /** 握手中途放弃时释放。 */
  free(): void
}

export interface Crypto {
  generateKeypair(): Keypair
  keypairFromSecret(secret: Uint8Array): Keypair
  /** 配对握手 `Noise_XXpsk3`。 */
  pairing(role: Role, local: Keypair, psk: Uint8Array): Handshake
  /** 重连握手 `Noise_IK`。initiator 必须给 `remotePublic`，responder 不给。 */
  reconnect(role: Role, local: Keypair, remotePublic?: Uint8Array): Handshake
  /**
   * 短码 → `{ matchId, psk }`，**配对路径用它，只算一次**。一次 Argon2id（`salt = "anycon-pair-v2"`，
   * m = 64 MiB、t = 3、p = 1，输出 64 字节）：前 8 字节的 hex 是 hub 撮合用的 `matchId`（16 个 hex，hub 永远看不到短码
   * 本身），后 32 字节是 PSK。8 位或 16 位短码都合法，否则抛错。
   *
   * 耗时：Node / 本机 wasm 约 0.1–0.3 s，浏览器 wasm 约 0.1–1 s，会同步占用线程（UI 要先显示「正在派生密钥…」
   * 再调用）。返回的 `psk` 是拷贝，调用方用完 `fill(0)`。
   */
  derivePairSecrets(code: string): PairSecrets
  /** 短码 → PSK（一次完整 Argon2id，约等于 `derivePairSecrets` 的耗时）。配对请直接用 `derivePairSecrets`，别和 `deriveMatchId` 各算一遍。 */
  derivePsk(code: string): Uint8Array
  /** 短码 → hub 撮合用的 match id（16 个 hex；一次完整 Argon2id）。8 位或 16 位都合法，否则抛错。 */
  deriveMatchId(code: string): string
  /** 大写，去掉 `-` 和空白。 */
  normalizeCode(code: string): string
  /** 生成 `XXXX-XXXX` 形式的 8 位短码（`anycon pair --short`，约 40 bit，只适合自建 hub）。 */
  generatePairCode(): string
  /** 生成 `XXXX-XXXX-XXXX-XXXX` 形式的 16 位短码（约 79 bit，`anycon pair` 的默认长度）。 */
  generatePairCodeLong(): string
  /** 公钥指纹：`3f:a2:…`。 */
  fingerprint(publicKey: Uint8Array): string
}

/** 一方的 CPace 状态（账号内接入，ADR-36）。新设备用 `'initiator'`，daemon 用 `'responder'`。 */
export interface Cpace {
  /** 自己的份额（32 字节），放进 `EnrollFrame.cpace` / `share` 发给对方。 */
  readonly share: Uint8Array
  /** 用对方的份额得到 32 字节 PSK（喂给 `pairing`）。份额不合法时抛 `invalid key`。之后这个对象不能再用。 */
  finish(role: Role, peerShare: Uint8Array): Uint8Array
  /** 中途放弃时释放（私有标量随之清零）。 */
  free(): void
}

/**
 * 账号内接入（ADR-36，protocol.md § 4.4）用到的密码学。`loadCrypto()` 返回的对象带这些方法；
 * 只做配对 / 重连的 fake 可以只实现 `Crypto`。
 */
export interface EnrollCrypto {
  /** `prs`：口令的 `deriveEnrollPasswordPrs`，或规范化后 6 位码的 ASCII 字节；`ci`：`enrollChannelId`；`sid`：`enrollSid`。 */
  cpace(prs: Uint8Array, ci: Uint8Array, sid: Uint8Array): Cpace
  /** 16 字节随机数（`EnrollHello.nonce`）。 */
  generateEnrollNonce(): Uint8Array
  /** 32 字节扫码密钥（放进二维码）。 */
  generateEnrollSecret(): Uint8Array
  /** 去掉空白和 `-`，必须是 6 个数字，否则抛错。 */
  normalizeEnrollCode(code: string): string
  /** 设置口令时校验：NFC 之后至少 8 个字符，否则抛 `enroll password too short`。 */
  validateEnrollPassword(password: string): void
  /** `Argon2id(NFC(口令), salt)`，浏览器里约 0.5–1 s，会同步占用线程。`salt` 是 `EnrollReply.salt`（16 字节）。 */
  deriveEnrollPasswordPrs(password: string, salt: Uint8Array): Uint8Array
  /** 扫码接入的 PSK：`HKDF-BLAKE2s(secret, info = "anycon-enroll-scan-v1" ‖ nodeId)`。 */
  deriveEnrollScanPsk(secret: Uint8Array, nodeId: string): Uint8Array
  /** CPace 信道标识，`method` 是 `'code'` 或 `'password'`。 */
  enrollChannelId(nodeId: string, method: string): Uint8Array
  /** CPace 会话标识：新设备的 nonce ‖ daemon 的 nonce。 */
  enrollSid(initiatorNonce: Uint8Array, responderNonce: Uint8Array): Uint8Array
}

/** `loadCrypto()` 的结果：配对 / 重连 / 传输 + 账号内接入。 */
export type FullCrypto = Crypto & EnrollCrypto

/** dev server 预览隧道（ADR-39）用到的部分。只有预览页的 service worker 用；`loadCrypto()` 的结果同时满足它。 */
export interface PreviewCrypto {
  /**
   * 预览隧道握手 `Noise_NKpsk0` 的 initiator：没有静态密钥，钉住 daemon 静态公钥 `daemonPublic`（32 字节）。
   * `psk` 是预览票据密钥（32 字节），`prologue` 用 `previewPrologue(label, ticketId)` 算。
   */
  previewInitiator(daemonPublic: Uint8Array, psk: Uint8Array, prologue: Uint8Array): Handshake
}

/** wasm 产物不存在或加载失败。 */
export class CryptoUnavailableError extends Error {
  override readonly name = 'CryptoUnavailableError'
}

// ---------- wasm-bindgen 产物的形状（只列我们用到的） ----------

interface RawKeypair {
  readonly public: Uint8Array
  readonly secret: Uint8Array
  free(): void
}
interface RawTransport {
  encrypt(plaintext: Uint8Array): Uint8Array
  decrypt(ciphertext: Uint8Array): Uint8Array
  readonly remotePublic: Uint8Array | (() => Uint8Array)
  free(): void
}
interface RawHandshake {
  readonly isMyTurn: boolean | (() => boolean)
  write(payload: Uint8Array): Uint8Array
  read(message: Uint8Array): Uint8Array
  readonly isFinished: boolean | (() => boolean)
  readonly remotePublic: Uint8Array | undefined | (() => Uint8Array | undefined)
  intoTransport(): RawTransport
  free(): void
}
interface RawCpace {
  readonly share: Uint8Array
  finish(role: Role, peerShare: Uint8Array): Uint8Array
  free(): void
}
interface RawPairSecrets {
  readonly matchId: string
  readonly psk: Uint8Array
  free(): void
}
/** 动态 import 得到的模块。 */
export interface WasmModule {
  default: (input?: unknown) => Promise<unknown>
  WasmKeypair: {
    generate(): RawKeypair
    fromSecret(secret: Uint8Array): RawKeypair
  }
  WasmHandshake: {
    pairing(role: Role, local: RawKeypair, psk: Uint8Array): RawHandshake
    reconnect(role: Role, local: RawKeypair, remotePublic?: Uint8Array): RawHandshake
    previewInitiator(daemonPublic: Uint8Array, psk: Uint8Array, prologue: Uint8Array): RawHandshake
  }
  derivePairSecrets(code: string): RawPairSecrets
  derivePsk(code: string): Uint8Array
  deriveMatchId(code: string): string
  normalizeCode(code: string): string
  generatePairCode(): string
  generatePairCodeLong(): string
  fingerprint(publicKey: Uint8Array): string
  WasmCpace: new (prs: Uint8Array, ci: Uint8Array, sid: Uint8Array) => RawCpace
  generateEnrollNonce(): Uint8Array
  generateEnrollSecret(): Uint8Array
  normalizeEnrollCode(code: string): string
  validateEnrollPassword(password: string): void
  deriveEnrollPasswordPrs(password: string, salt: Uint8Array): Uint8Array
  deriveEnrollScanPsk(secret: Uint8Array, nodeId: string): Uint8Array
  enrollChannelId(nodeId: string, method: string): Uint8Array
  enrollSid(initiatorNonce: Uint8Array, responderNonce: Uint8Array): Uint8Array
}

/** `.wasm` 的来源：URL / 字节 / 已编译的 Module / fetch 的 Response。 */
export type WasmSource =
  | string
  | URL
  | BufferSource
  | WebAssembly.Module
  | Response
  | Promise<Response | BufferSource | WebAssembly.Module>

export interface LoadCryptoOptions {
  /**
   * `.wasm` 的位置。浏览器里交给打包器：
   * `loadCrypto({ wasm: new URL('@anycon/protocol/wasm/anycon_crypto_bg.wasm', import.meta.url) })`
   * 或 Vite 的 `?url` 导入。Node / Bun 下不给就自动读 `../wasm/anycon_crypto_bg.wasm`。
   */
  wasm?: WasmSource
  /** 直接给 wasm-bindgen 的 JS 模块（打包器想自己控制 import 路径时用）。 */
  module?: WasmModule | Promise<WasmModule>
}

function member<T>(obj: object, name: string): T {
  const value = (obj as Record<string, unknown>)[name]
  return (typeof value === 'function' ? (value as () => T).call(obj) : value) as T
}

function wrapTransport(raw: RawTransport): Transport {
  const remote = member<Uint8Array>(raw, 'remotePublic').slice()
  return {
    encrypt: (plaintext) => raw.encrypt(plaintext),
    decrypt: (ciphertext) => raw.decrypt(ciphertext),
    remotePublic: remote,
    free: () => raw.free(),
  }
}

function wrapHandshake(raw: RawHandshake): Handshake {
  return {
    isMyTurn: () => member<boolean>(raw, 'isMyTurn'),
    write: (payload) => raw.write(payload ?? new Uint8Array(0)),
    read: (message) => raw.read(message),
    isFinished: () => member<boolean>(raw, 'isFinished'),
    remotePublic: () => {
      const key = member<Uint8Array | undefined | null>(raw, 'remotePublic')
      return key ? key.slice() : undefined
    },
    intoTransport: () => wrapTransport(raw.intoTransport()),
    free: () => raw.free(),
  }
}

function toKeypair(raw: RawKeypair): Keypair {
  const kp: Keypair = { public: raw.public.slice(), secret: raw.secret.slice() }
  raw.free()
  return kp
}

function wrapCpace(raw: RawCpace): Cpace {
  let done = false
  return {
    share: raw.share.slice(),
    // wasm 侧 `finish` 按值消费对象，之后 `free` 是空操作
    finish: (role, peerShare) => {
      done = true
      return raw.finish(role, peerShare)
    },
    free: () => {
      if (!done) raw.free()
      done = true
    },
  }
}

function wrapModule(mod: WasmModule): FullCrypto & PreviewCrypto {
  // 传给 wasm 的 Keypair 每次临时重建。握手对象建好后释放它：wasm 若按值消费了它，free 是空操作。
  const withKeypair = (kp: Keypair, build: (raw: RawKeypair) => RawHandshake): Handshake => {
    const raw = mod.WasmKeypair.fromSecret(kp.secret)
    try {
      return wrapHandshake(build(raw))
    } finally {
      try {
        raw.free()
      } catch {
        // 已被消费
      }
    }
  }
  return {
    generateKeypair: () => toKeypair(mod.WasmKeypair.generate()),
    keypairFromSecret: (secret) => toKeypair(mod.WasmKeypair.fromSecret(secret)),
    pairing: (role, local, psk) =>
      withKeypair(local, (raw) => mod.WasmHandshake.pairing(role, raw, psk)),
    reconnect: (role, local, remotePublic) =>
      withKeypair(local, (raw) => mod.WasmHandshake.reconnect(role, raw, remotePublic)),
    previewInitiator: (daemonPublic, psk, prologue) =>
      wrapHandshake(mod.WasmHandshake.previewInitiator(daemonPublic, psk, prologue)),
    derivePairSecrets: (code) => {
      const raw = mod.derivePairSecrets(code)
      try {
        // `psk` getter 每次返回一份新的 JS 拷贝（wasm 内存里的原件随 `free()` 清零），所以直接交给调用方，
        // 不再 `.slice()` 多复制一份——多出来的那份没人能清零。调用方用完要 `psk.fill(0)`。
        return { matchId: raw.matchId, psk: raw.psk }
      } finally {
        raw.free()
      }
    },
    derivePsk: (code) => mod.derivePsk(code),
    deriveMatchId: (code) => mod.deriveMatchId(code),
    normalizeCode: (code) => mod.normalizeCode(code),
    generatePairCode: () => mod.generatePairCode(),
    generatePairCodeLong: () => mod.generatePairCodeLong(),
    fingerprint: (publicKey) => mod.fingerprint(publicKey),
    cpace: (prs, ci, sid) => wrapCpace(new mod.WasmCpace(prs, ci, sid)),
    generateEnrollNonce: () => mod.generateEnrollNonce(),
    generateEnrollSecret: () => mod.generateEnrollSecret(),
    normalizeEnrollCode: (code) => mod.normalizeEnrollCode(code),
    validateEnrollPassword: (password) => mod.validateEnrollPassword(password),
    deriveEnrollPasswordPrs: (password, salt) => mod.deriveEnrollPasswordPrs(password, salt),
    deriveEnrollScanPsk: (secret, nodeId) => mod.deriveEnrollScanPsk(secret, nodeId),
    enrollChannelId: (nodeId, method) => mod.enrollChannelId(nodeId, method),
    enrollSid: (a, b) => mod.enrollSid(a, b),
  }
}

const isNode = (): boolean =>
  typeof (globalThis as { process?: { versions?: { node?: string } } }).process?.versions?.node ===
  'string'

async function importWasmModule(): Promise<WasmModule> {
  try {
    // 字面量路径：打包器（Vite 等）能静态分析并把 wasm 胶水 js 打进来。
    // @ts-ignore -- 产物由 `just gen-wasm` 生成，可能不存在，类型也由本文件的 WasmModule 描述。
    return (await import('../wasm/anycon_crypto.js')) as unknown as WasmModule
  } catch (cause) {
    throw new CryptoUnavailableError(
      'anycon crypto wasm is not built: packages/protocol/wasm/anycon_crypto.js is missing. ' +
        'Run `just gen-wasm` (wasm-pack build of crates/crypto) first.',
      { cause },
    )
  }
}

async function nodeWasmBytes(): Promise<Uint8Array> {
  // 变量指定符：不让浏览器打包器解析 node 内置模块。
  const fsSpecifier = 'node:fs/promises'
  const { readFile } = (await import(/* @vite-ignore */ fsSpecifier)) as {
    readFile(url: URL): Promise<Uint8Array>
  }
  const url = new URL('../wasm/anycon_crypto_bg.wasm', import.meta.url)
  try {
    return await readFile(url)
  } catch (cause) {
    throw new CryptoUnavailableError(
      `anycon crypto wasm binary not found at ${url.pathname}. Run \`just gen-wasm\`.`,
      { cause },
    )
  }
}

async function initialize(options: LoadCryptoOptions): Promise<FullCrypto & PreviewCrypto> {
  const mod = await (options.module ?? importWasmModule())
  const input: unknown = options.wasm ?? (isNode() ? await nodeWasmBytes() : undefined)
  // wasm-bindgen ≥ 0.2.93 的对象形参；不给 input 时浏览器走胶水 js 里的 `new URL(…, import.meta.url)`。
  await mod.default(input === undefined ? undefined : { module_or_path: input })
  return wrapModule(mod)
}

let cached: Promise<FullCrypto & PreviewCrypto> | undefined

/**
 * 加载并初始化 wasm。进程内只初始化一次（第二次调用返回同一个 Promise；失败不缓存，可重试）。
 * wasm 产物不存在时抛 `CryptoUnavailableError`，消息里写明要跑 `just gen-wasm`。
 */
export function loadCrypto(options: LoadCryptoOptions = {}): Promise<FullCrypto & PreviewCrypto> {
  if (!cached) {
    cached = initialize(options).catch((err: unknown) => {
      cached = undefined
      throw err
    })
  }
  return cached
}

/** 短码规范化（纯 JS）：大写，去掉 `-` 和空白。与 `crates/crypto` 的 `normalize_code` 一致。 */
export function normalizePairCode(code: string): string {
  return code.replace(/[\s-]/g, '').toUpperCase()
}

/** 短码格式校验：规范化后是 8 位或 16 位，且都在字母表内。纯 JS，不需要 wasm。 */
export function isValidPairCode(code: string): boolean {
  const normalized = normalizePairCode(code)
  if (normalized.length !== PAIR_CODE_LEN && normalized.length !== PAIR_CODE_LEN_LONG) return false
  for (const ch of normalized) if (!PAIR_CODE_ALPHABET.includes(ch)) return false
  return true
}

/**
 * `XXXXXXXX` → `XXXX-XXXX`，16 位 → `XXXX-XXXX-XXXX-XXXX`（每 4 位一组）。
 * 长度既不是 8 也不是 16 时原样返回规范化结果。
 */
export function formatPairCode(code: string): string {
  const normalized = normalizePairCode(code)
  if (normalized.length !== PAIR_CODE_LEN && normalized.length !== PAIR_CODE_LEN_LONG) {
    return normalized
  }
  return normalized.match(/.{4}/g)!.join('-')
}

/**
 * 从用户输入里取出短码：接受裸短码（8 / 16 位，大小写、`-`、空白随意）、完整配对链接
 * `https://hub.example/pair#code=XXXX-XXXX`、或只有 fragment 的 `#code=…` / `code=…`。
 * 短码在 fragment 里，浏览器不会把它发给 hub。格式不合法返回 `undefined`。
 */
export function extractPairCode(input: string): string | undefined {
  const text = input.trim()
  if (text === '') return undefined
  let candidate = text
  const hashAt = text.indexOf('#')
  if (hashAt >= 0) {
    candidate = new URLSearchParams(text.slice(hashAt + 1)).get('code') ?? ''
  } else if (/^code=/i.test(text)) {
    candidate = new URLSearchParams(text).get('code') ?? ''
  }
  return isValidPairCode(candidate) ? formatPairCode(candidate) : undefined
}

export function hexOf(bytes: Uint8Array): string {
  let out = ''
  for (const b of bytes) out += b.toString(16).padStart(2, '0')
  return out
}

export function bytesOfHex(hex: string): Uint8Array {
  if (hex.length % 2 !== 0 || /[^0-9a-fA-F]/.test(hex)) throw new Error('invalid hex string')
  const out = new Uint8Array(hex.length / 2)
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16)
  return out
}
