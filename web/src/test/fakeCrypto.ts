// 内存假 Noise：实现与 wasm 同一接口，只用于单元测试（不安全，不要在生产里引用）。
// 保留真实握手的形状：XX 三条（initiator / responder / initiator），IK 两条；
// PSK 不对、responder 公钥不符、传输层 nonce 乱序都会抛错。
import type { Cpace, FullCrypto, Handshake, Keypair, Role, Transport } from '@/transport/crypto'

function rnd(n: number): Uint8Array {
  const b = new Uint8Array(n)
  for (let i = 0; i < n; i++) b[i] = Math.floor(Math.random() * 256)
  return b
}

function eq(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((x, i) => x === b[i])
}

function tagOf(psk: Uint8Array): number {
  return psk.reduce((acc, x) => (acc * 31 + x) & 0xff, 7)
}

/** XXpsk3 第 3 条里带的 PSK 校验值：FNV-1a 32 位。一个字节的 `tagOf` 在「错口令一定握手失败」的用例里会有 1/256 的假通过。 */
function pskTag(psk: Uint8Array): Uint8Array {
  let h = 0x811c9dc5
  for (const b of psk) h = Math.imul(h ^ b, 0x01000193) >>> 0
  return Uint8Array.of(h >>> 24, (h >>> 16) & 0xff, (h >>> 8) & 0xff, h & 0xff)
}

/** 假传输：XOR + 一个字节的校验和（模拟 AEAD：改一个字节就解不开）+ 顺序 nonce。 */
class FakeTransport implements Transport {
  private sendN = 0
  private recvN = 0
  private readonly key: number
  constructor(
    local: Uint8Array,
    private readonly remote: Uint8Array,
    salt = 0,
  ) {
    this.key = (local[0]! ^ remote[0]! ^ salt) & 0xff
  }
  private mac(body: Uint8Array, n: number): number {
    let sum = this.key * 7 + n
    for (const b of body) sum = (sum * 31 + b) & 0xffff
    return sum & 0xff
  }
  encrypt(pt: Uint8Array): Uint8Array {
    const out = new Uint8Array(6 + pt.length)
    out[0] = 0xe0
    new DataView(out.buffer).setUint32(1, this.sendN)
    for (let i = 0; i < pt.length; i++) out[6 + i] = pt[i]! ^ this.key
    out[5] = this.mac(out.subarray(6), this.sendN)
    this.sendN++
    return out
  }
  decrypt(ct: Uint8Array): Uint8Array {
    if (ct.length < 6 || ct[0] !== 0xe0) throw new Error('decrypt failed: malformed')
    const n = new DataView(ct.buffer, ct.byteOffset, ct.byteLength).getUint32(1)
    if (n !== this.recvN) throw new Error('decrypt failed: nonce')
    if (ct[5] !== this.mac(ct.subarray(6), n)) throw new Error('decrypt failed: bad mac')
    this.recvN++
    const out = new Uint8Array(ct.length - 6)
    for (let i = 0; i < out.length; i++) out[i] = ct[6 + i]! ^ this.key
    return out
  }
  get remotePublic(): Uint8Array {
    return this.remote
  }
  free(): void {}
}

type Kind = 'xx' | 'ik'

class FakeHandshake implements Handshake {
  private step = 0
  private remote: Uint8Array | undefined
  /** 模拟握手临时密钥：initiator 随机生成，responder 在回复里回显，initiator 校验——回复只能配给发出它的那次握手（真实 Noise 里是 ee / se 的 DH）。 */
  private nonce: Uint8Array | undefined
  /** 和真实实现一样把 PSK 拷走：调用方拿到握手对象后会把自己那份清零。 */
  private readonly psk: Uint8Array | undefined
  constructor(
    private readonly kind: Kind,
    private readonly role: Role,
    private readonly local: Keypair,
    psk: Uint8Array | undefined,
    private readonly expectedRemote: Uint8Array | undefined,
  ) {
    this.psk = psk && new Uint8Array(psk)
    if (kind === 'ik' && role === 'initiator') {
      if (!expectedRemote) throw new Error('IK initiator needs remote public')
      this.remote = expectedRemote
    }
  }
  private get total(): number {
    return this.kind === 'xx' ? 3 : 2
  }
  isMyTurn(): boolean {
    if (this.isFinished()) return false
    return this.role === 'initiator' ? this.step % 2 === 0 : this.step % 2 === 1
  }
  write(payload: Uint8Array = new Uint8Array(0)): Uint8Array {
    if (!this.isMyTurn()) throw new Error('not my turn')
    const i = this.step++
    const cat = (...parts: Array<Uint8Array | number[]>): Uint8Array => Uint8Array.from(parts.flatMap((p) => Array.from(p)))
    if (this.kind === 'xx') {
      if (i === 0) return cat([0x01], (this.nonce = rnd(4)), payload)
      if (i === 1) return cat([0x02], this.local.public, this.nonce!, payload)
      return cat([0x03], pskTag(this.psk!), this.local.public, payload)
    }
    // IK 的第一条是加密给 responder 静态公钥的：tag 模拟「只有持有对应私钥的一方读得出来」
    if (i === 0) return cat([0x11], this.local.public, (this.nonce = rnd(4)), [tagOf(this.remote!)], payload)
    return cat([0x12], this.local.public, this.nonce!, payload)
  }
  read(msg: Uint8Array): Uint8Array {
    if (this.isMyTurn() || this.isFinished()) throw new Error('not my turn to read')
    const i = this.step++
    let payload = new Uint8Array(0)
    if (this.kind === 'xx') {
      if (i === 1 && msg[0] === 0x02) {
        if (!eq(msg.slice(33, 37), this.nonce!)) throw new Error('handshake failed: reply does not belong to this handshake')
        this.remote = msg.slice(1, 33)
        payload = msg.slice(37)
      } else if (i === 2 && msg[0] === 0x03) {
        if (!eq(msg.slice(1, 5), pskTag(this.psk!))) throw new Error('handshake failed: bad psk')
        this.remote = msg.slice(5, 37)
        payload = msg.slice(37)
      } else if (i !== 0 || msg[0] !== 0x01) throw new Error('handshake failed: unexpected message')
      else {
        this.nonce = msg.slice(1, 5)
        payload = msg.slice(5)
      }
    } else if (i === 0) {
      if (msg[0] !== 0x11) throw new Error('handshake failed: unexpected message')
      const remote = msg.slice(1, 33)
      if (this.expectedRemote && !eq(remote, this.expectedRemote)) throw new Error('handshake failed: unknown static key')
      if (msg[37] !== tagOf(this.local.public)) throw new Error('handshake failed: not encrypted to my static key')
      this.remote = remote
      this.nonce = msg.slice(33, 37)
      payload = msg.slice(38)
    } else {
      if (msg[0] !== 0x12 || !eq(msg.slice(1, 33), this.expectedRemote!)) throw new Error('handshake failed: bad responder key')
      if (!eq(msg.slice(33, 37), this.nonce!)) throw new Error('handshake failed: reply does not belong to this handshake')
      payload = msg.slice(37)
    }
    return payload
  }
  isFinished(): boolean {
    return this.step >= this.total
  }
  remotePublic(): Uint8Array | undefined {
    return this.remote
  }
  intoTransport(): Transport {
    if (!this.isFinished() || !this.remote) throw new Error('handshake not finished')
    // XXpsk3 的传输密钥里混着 PSK：PSK 不同则互相解不开（IK 没有 PSK）
    // 传输密钥也绑定这次握手的临时密钥：用别的握手的回复建出来的传输解不开对端的数据
    const salt = ((this.psk ? tagOf(this.psk) : 0) ^ Array.from(this.nonce ?? []).reduce((a, b) => (a * 31 + b) & 0xff, 0)) & 0xff
    return new FakeTransport(this.local.public, this.remote, salt)
  }
  free(): void {}
}

function kp(secret: Uint8Array): Keypair {
  // 公钥 = secret 逐字节取反，纯粹为了「由 secret 可复现」
  return { secret, public: secret.map((x) => x ^ 0xff) }
}

/** 假 CPace：两边 prs / ci / sid 都一样才得到同一个 PSK，否则 PSK 不同、XXpsk3 第 3 条校验失败（和真实实现一样的可观察行为）。 */
class FakeCpace implements Cpace {
  readonly share = rnd(32)
  constructor(
    private readonly prs: Uint8Array,
    private readonly ci: Uint8Array,
    private readonly sid: Uint8Array,
  ) {}
  finish(role: Role, peerShare: Uint8Array): Uint8Array {
    if (peerShare.length !== 32) throw new Error('invalid key')
    const [first, second] = role === 'initiator' ? [this.share, peerShare] : [peerShare, this.share]
    return expand(32, this.prs, this.ci, this.sid, first, second)
  }
  free(): void {}
}

/** 确定性的「哈希」展开：把若干段字节混成 n 字节（只用于测试里让不同输入得到不同输出）。 */
function expand(n: number, ...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(n)
  let h = 0x811c9dc5
  for (const p of parts) {
    h = Math.imul(h ^ p.length, 0x01000193) >>> 0
    for (const b of p) h = Math.imul(h ^ b, 0x01000193) >>> 0
  }
  for (let i = 0; i < n; i++) {
    h = Math.imul(h ^ (i + 1), 0x01000193) >>> 0
    h ^= h >>> 15
    out[i] = h & 0xff
  }
  return out
}

const text = (s: string): Uint8Array => new TextEncoder().encode(s)

export function createFakeCrypto(): FullCrypto {
  const crypto: FullCrypto = {
    generateKeypair: () => kp(rnd(32)),
    keypairFromSecret: (s) => kp(new Uint8Array(s)),
    pairing: (role, local, psk) => new FakeHandshake('xx', role, local, psk, undefined),
    reconnect: (role, local, remote) => new FakeHandshake('ik', role, local, undefined, remote),
    derivePsk: (c) => {
      const n = c.replace(/[\s-]/g, '').toUpperCase()
      if (n.length !== 8 && n.length !== 16) throw new Error('invalid pair code')
      const out = new Uint8Array(32)
      for (let i = 0; i < n.length; i++) out[i % 32] = (out[i % 32]! + n.charCodeAt(i)) & 0xff
      return out
    },
    normalizeCode: (c) => c.replace(/[\s-]/g, '').toUpperCase(),
    deriveMatchId: (c) => {
      const n = c.replace(/[\s-]/g, '').toUpperCase()
      if (n.length !== 8 && n.length !== 16) throw new Error('invalid pair code')
      // 和 psk 用不同的混合方式：match id 推不出 psk
      let h = 0x811c9dc5
      for (let i = 0; i < n.length; i++) h = Math.imul(h ^ n.charCodeAt(i), 0x01000193) >>> 0
      return h.toString(16).padStart(8, '0').repeat(2)
    },
    // 真实实现是一次 Argon2id 同时出 matchId 和 psk；假的就把两个假派生拼起来，每次返回新数组
    derivePairSecrets: (c) => ({ matchId: crypto.deriveMatchId(c), psk: crypto.derivePsk(c) }),
    generatePairCode: () => 'ABCD-2345',
    generatePairCodeLong: () => 'ABCD-2345-WXYZ-6789',
    fingerprint: (p) =>
      Array.from(p.slice(0, 8), (x) => x.toString(16).padStart(2, '0')).join(':'),
    // ---- 账号内接入（ADR-36）：行为对齐 crates/crypto 的 enroll.rs，实现是假的 ----
    cpace: (prs, ci, sid) => new FakeCpace(new Uint8Array(prs), new Uint8Array(ci), new Uint8Array(sid)),
    generateEnrollNonce: () => rnd(16),
    generateEnrollSecret: () => rnd(32),
    normalizeEnrollCode: (c) => {
      const n = c.replace(/[\s-]/g, '')
      if (!/^[0-9]{6}$/.test(n)) throw new Error('invalid enroll code')
      return n
    },
    validateEnrollPassword: (p) => {
      if ([...p.normalize('NFC')].length < 8) throw new Error('enroll password too short')
    },
    deriveEnrollPasswordPrs: (p, salt) => {
      if (salt.length !== 16) throw new Error('salt must be 16 bytes')
      return expand(32, text(p.normalize('NFC')), salt)
    },
    deriveEnrollScanPsk: (secret, nodeId) => expand(32, secret, text('anycon-enroll-scan-v1'), text(nodeId)),
    enrollChannelId: (nodeId, method) => text(`anycon-enroll-v1|${nodeId}|${method}`),
    enrollSid: (a, b) => Uint8Array.from([...a, ...b]),
  }
  return crypto
}
