// 内存假 Noise（NKpsk0 的形状），只用于单元测试，不安全，不要在生产里引用。
// 保留真实握手的可观察行为：initiator 一条、responder 一条；PSK、prologue、钉住的公钥不对都会让握手失败；
// 传输层有顺序 nonce 和校验，乱序 / 篡改会抛错。两端都从这里取，所以测试里能搭一个假 daemon。
import type { Handshake, PreviewCrypto, Transport } from '@anycon/protocol'

function fnv(...parts: Uint8Array[]): number {
  let h = 0x811c9dc5
  for (const p of parts) {
    h = Math.imul(h ^ p.length, 0x01000193) >>> 0
    for (const b of p) h = Math.imul(h ^ b, 0x01000193) >>> 0
  }
  return h >>> 0
}

const u32 = (n: number): Uint8Array => Uint8Array.of(n >>> 24, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff)
const eq = (a: Uint8Array, b: Uint8Array): boolean => a.length === b.length && a.every((x, i) => x === b[i])
const cat = (...parts: Uint8Array[]): Uint8Array => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0))
  let at = 0
  for (const p of parts) {
    out.set(p, at)
    at += p.length
  }
  return out
}

let counter = 1
const nextEph = (): Uint8Array => u32(Math.imul(counter++, 2654435761) >>> 0)

/** 传输层：每条消息 `0xe0 ‖ nonce(4) ‖ mac(1) ‖ XOR 过的明文`，开销固定 6 字节。 */
class FakeTransport implements Transport {
  private sendN = 0
  private recvN = 0
  constructor(
    private readonly key: number,
    readonly remotePublic: Uint8Array,
  ) {}
  /** 校验字节：只看长度和首尾各 32 字节（测试里的大消息很多，逐字节算太慢）。 */
  private mac(body: Uint8Array, n: number): number {
    const head = body.subarray(0, 32)
    const tail = body.subarray(Math.max(32, body.length - 32))
    return (fnv(u32(this.key), u32(n), u32(body.length), head, tail) >>> 8) & 0xff
  }
  private xor(src: Uint8Array): Uint8Array {
    const out = new Uint8Array(src.length)
    const k = [this.key & 0xff, (this.key >>> 8) & 0xff, (this.key >>> 16) & 0xff, (this.key >>> 24) & 0xff]
    for (let i = 0; i < src.length; i++) out[i] = src[i]! ^ k[i & 3]!
    return out
  }
  encrypt(plaintext: Uint8Array): Uint8Array {
    const n = this.sendN++
    const body = this.xor(plaintext)
    return cat(Uint8Array.of(0xe0), u32(n), Uint8Array.of(this.mac(body, n)), body)
  }
  decrypt(ciphertext: Uint8Array): Uint8Array {
    if (ciphertext.length < 6 || ciphertext[0] !== 0xe0) throw new Error('decrypt failed: malformed')
    const n = new DataView(ciphertext.buffer, ciphertext.byteOffset, ciphertext.byteLength).getUint32(1)
    if (n !== this.recvN) throw new Error('decrypt failed: nonce')
    const body = ciphertext.subarray(6)
    if (ciphertext[5] !== this.mac(body, n)) throw new Error('decrypt failed: bad mac')
    this.recvN++
    return this.xor(body)
  }
  free(): void {}
}

class FakeHandshake implements Handshake {
  private step = 0
  private eph: Uint8Array = new Uint8Array(0)
  private remote: Uint8Array | undefined
  private readonly seed: number

  constructor(
    private readonly role: 'initiator' | 'responder',
    /** initiator：钉住的 daemon 公钥；responder：自己的静态公钥。 */
    private readonly daemonPublic: Uint8Array,
    psk: Uint8Array,
    prologue: Uint8Array,
  ) {
    this.seed = fnv(psk, prologue, daemonPublic)
    if (role === 'initiator') this.remote = daemonPublic
  }

  isMyTurn(): boolean {
    if (this.isFinished()) return false
    return this.role === 'initiator' ? this.step === 0 : this.step === 1
  }

  write(payload: Uint8Array = new Uint8Array(0)): Uint8Array {
    if (!this.isMyTurn()) throw new Error('not my turn')
    this.step++
    if (this.role === 'initiator') {
      this.eph = nextEph()
      return cat(Uint8Array.of(0xa1), this.eph, u32(fnv(u32(this.seed), this.eph)))
    }
    return cat(Uint8Array.of(0xa2), this.eph, u32(fnv(u32(this.seed), this.eph, payload)), payload)
  }

  read(message: Uint8Array): Uint8Array {
    if (this.isMyTurn() || this.isFinished()) throw new Error('not my turn to read')
    this.step++
    if (this.role === 'responder') {
      if (message[0] !== 0xa1 || message.length !== 9) throw new Error('handshake failed: unexpected message')
      const eph = message.slice(1, 5)
      if (!eq(message.slice(5, 9), u32(fnv(u32(this.seed), eph)))) throw new Error('handshake failed: bad psk or prologue')
      this.eph = eph
      this.remote = Uint8Array.of(0xcc) // 预览 initiator 没有静态密钥
      return new Uint8Array(0)
    }
    if (message[0] !== 0xa2 || message.length < 9) throw new Error('handshake failed: unexpected message')
    const payload = message.slice(9)
    if (!eq(message.slice(1, 5), this.eph)) throw new Error('handshake failed: reply does not belong to this handshake')
    if (!eq(message.slice(5, 9), u32(fnv(u32(this.seed), this.eph, payload)))) {
      throw new Error('handshake failed: responder is not the pinned daemon')
    }
    return payload
  }

  isFinished(): boolean {
    return this.step >= 2
  }
  remotePublic(): Uint8Array | undefined {
    return this.remote
  }
  intoTransport(): Transport {
    if (!this.isFinished()) throw new Error('handshake not finished')
    return new FakeTransport(fnv(u32(this.seed), this.eph), this.remote ?? this.daemonPublic)
  }
  free(): void {}
}

/** 浏览器一侧（预览页）：和 `loadCrypto()` 的 `previewInitiator` 同一个接口。 */
export function createFakePreviewCrypto(): PreviewCrypto {
  return {
    previewInitiator: (daemonPublic, psk, prologue) => new FakeHandshake('initiator', daemonPublic, psk.slice(), prologue),
  }
}

/** daemon 一侧：钉住的公钥就是自己的静态公钥。 */
export function fakePreviewResponder(daemonPublic: Uint8Array, psk: Uint8Array, prologue: Uint8Array): Handshake {
  return new FakeHandshake('responder', daemonPublic, psk.slice(), prologue)
}

/** 直接搭一对已经握好手的传输：session 级测试用。 */
export function fakeTransportPair(): { page: Transport; daemon: Transport } {
  const pub = new Uint8Array(32).fill(7)
  const psk = new Uint8Array(32).fill(9)
  const prologue = Uint8Array.of(1, 2, 3)
  const i = createFakePreviewCrypto().previewInitiator(pub, psk, prologue)
  const r = fakePreviewResponder(pub, psk, prologue)
  r.read(i.write())
  i.read(r.write())
  return { page: i.intoTransport(), daemon: r.intoTransport() }
}
