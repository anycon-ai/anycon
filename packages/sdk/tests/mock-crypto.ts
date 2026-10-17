/**
 * 不安全的玩具 `Crypto` 实现，只用于在没有 wasm 的环境里测 SDK 的流程（握手消息个数 / 先后手 / 公钥核对 /
 * 传输计数器 / 短码 PSK 校验）。形状和真实 Noise 一致：XX 三条、IK 两条、Transport 按序计数、分片格式相同。
 * 真实密码学由 `crates/crypto` 的测试和 `packages/protocol/tests/crypto.test.ts` 覆盖。
 */
import { createHash, createHmac, randomBytes } from 'node:crypto'

import type { Crypto, Handshake, Keypair, Role, Transport } from '@anycon/protocol'

const ALPHABET = '23456789ABCDEFGHJKMNPQRSTUVWXYZ'

const sha = (...parts: Uint8Array[]) =>
  new Uint8Array(createHash('sha256').update(Buffer.concat(parts)).digest())
const mac = (key: Uint8Array, ...parts: Uint8Array[]) =>
  new Uint8Array(createHmac('sha256', key).update(Buffer.concat(parts)).digest()).slice(0, 16)
const text = (s: string) => new TextEncoder().encode(s)
const cat = (...parts: Uint8Array[]) => new Uint8Array(Buffer.concat(parts))
const eq = (a: Uint8Array, b: Uint8Array) => a.length === b.length && a.every((x, i) => x === b[i])

function publicOf(secret: Uint8Array): Uint8Array {
  return sha(text('pub'), secret)
}

function makeTransport(key: Uint8Array, role: Role, remote: Uint8Array): Transport {
  const sendKey = sha(key, text(role === 'initiator' ? 'i2r' : 'r2i'))
  const recvKey = sha(key, text(role === 'initiator' ? 'r2i' : 'i2r'))
  let sendCtr = 0
  let recvCtr = 0
  const stream = (k: Uint8Array, ctr: number, len: number) => {
    const out = new Uint8Array(len)
    for (let off = 0, block = 0; off < len; block++) {
      const ks = sha(k, text(`${ctr}:${block}`))
      const n = Math.min(ks.length, len - off)
      out.set(ks.subarray(0, n), off)
      off += n
    }
    return out
  }
  const ctrBytes = (n: number) => text(String(n))
  return {
    encrypt(plaintext) {
      const frames: Uint8Array[] = []
      const chunks: Uint8Array[] = []
      for (let at = 0; at < plaintext.length; at += 65519)
        chunks.push(plaintext.subarray(at, at + 65519))
      if (chunks.length === 0) chunks.push(new Uint8Array(0))
      for (const chunk of chunks) {
        const ks = stream(sendKey, sendCtr, chunk.length)
        const ct = chunk.map((b, i) => b ^ ks[i]!)
        const tag = mac(sendKey, ctrBytes(sendCtr), ct)
        const len = ct.length + 16
        frames.push(Uint8Array.of(len >> 8, len & 0xff), ct, tag)
        sendCtr++
      }
      return cat(...frames)
    },
    decrypt(ciphertext) {
      if (ciphertext.length < 2) throw new Error('malformed frame')
      const parts: Uint8Array[] = []
      let at = 0
      let ctr = recvCtr
      while (at < ciphertext.length) {
        if (at + 2 > ciphertext.length) throw new Error('malformed frame')
        const len = (ciphertext[at]! << 8) | ciphertext[at + 1]!
        at += 2
        if (len < 16 || at + len > ciphertext.length) throw new Error('malformed frame')
        const ct = ciphertext.subarray(at, at + len - 16)
        const tag = ciphertext.subarray(at + len - 16, at + len)
        at += len
        if (!eq(tag, mac(recvKey, ctrBytes(ctr), ct))) throw new Error('decrypt failed')
        const ks = stream(recvKey, ctr, ct.length)
        parts.push(ct.map((b, i) => b ^ ks[i]!))
        ctr++
      }
      recvCtr = ctr // 失败的 decrypt 不推进计数器
      return cat(...parts)
    },
    remotePublic: remote,
    free() {},
  }
}

function pairingHandshake(role: Role, local: Keypair, psk: Uint8Array): Handshake {
  const initiator = role === 'initiator'
  const eph = randomBytes(8)
  let step = 0
  let remoteEph = new Uint8Array(8)
  let remote: Uint8Array | undefined
  let finished = false
  let transport: Transport | undefined
  const finish = (eI: Uint8Array, eR: Uint8Array, sI: Uint8Array, sR: Uint8Array) => {
    finished = true
    transport = makeTransport(sha(psk, sI, sR, eI, eR), role, initiator ? sR : sI)
  }
  return {
    isMyTurn: () => !finished && (initiator ? step % 2 === 0 : step % 2 === 1),
    isFinished: () => finished,
    remotePublic: () => remote,
    write(payload = new Uint8Array(0)) {
      if (initiator && step === 0) {
        step = 1
        return cat(Uint8Array.of(1), eph)
      }
      if (!initiator && step === 1) {
        step = 2
        return cat(Uint8Array.of(2), eph, local.public)
      }
      if (initiator && step === 2) {
        const tag = mac(psk, text('m3'), eph, remoteEph, local.public, remote!, payload)
        finish(eph, remoteEph, local.public, remote!)
        step = 3
        return cat(Uint8Array.of(3), local.public, tag, payload)
      }
      throw new Error('not your turn to write')
    },
    read(message) {
      if (!initiator && step === 0 && message[0] === 1) {
        remoteEph = message.slice(1, 9)
        step = 1
        return new Uint8Array(0)
      }
      if (initiator && step === 1 && message[0] === 2) {
        remoteEph = message.slice(1, 9)
        remote = message.slice(9, 41)
        step = 2
        return new Uint8Array(0)
      }
      if (!initiator && step === 2 && message[0] === 3) {
        const sI = message.slice(1, 33)
        const tag = message.slice(33, 49)
        const payload = message.slice(49)
        if (!eq(tag, mac(psk, text('m3'), remoteEph, eph, sI, local.public, payload))) {
          throw new Error('decrypt failed')
        }
        remote = sI
        finish(remoteEph, eph, sI, local.public)
        step = 3
        return payload
      }
      throw new Error('malformed frame')
    },
    intoTransport() {
      if (!transport) throw new Error('handshake not finished')
      return transport
    },
    free() {},
  }
}

function reconnectHandshake(role: Role, local: Keypair, expectedRemote?: Uint8Array): Handshake {
  const initiator = role === 'initiator'
  if (initiator && !expectedRemote) throw new Error('invalid key')
  const eph = randomBytes(8)
  let step = 0
  let remoteEph = new Uint8Array(8)
  let remote: Uint8Array | undefined = initiator ? expectedRemote : undefined
  let finished = false
  let transport: Transport | undefined
  const finish = (sI: Uint8Array, sR: Uint8Array, eI: Uint8Array, eR: Uint8Array) => {
    finished = true
    transport = makeTransport(sha(sI, sR, eI, eR, text('ik')), role, initiator ? sR : sI)
  }
  return {
    isMyTurn: () => !finished && (initiator ? step === 0 : step === 1),
    isFinished: () => finished,
    remotePublic: () => remote,
    write() {
      if (initiator && step === 0) {
        const tag = mac(sha(remote!), text('ik1'), eph, local.public)
        step = 1
        return cat(Uint8Array.of(0x11), eph, local.public, tag)
      }
      if (!initiator && step === 1) {
        const tag = mac(sha(remote!), text('ik2'), remoteEph, eph)
        finish(remote!, local.public, remoteEph, eph)
        step = 2
        return cat(Uint8Array.of(0x12), eph, tag)
      }
      throw new Error('not your turn to write')
    },
    read(message) {
      if (!initiator && step === 0 && message[0] === 0x11) {
        const eI = message.slice(1, 9)
        const sI = message.slice(9, 41)
        const tag = message.slice(41, 57)
        if (!eq(tag, mac(sha(local.public), text('ik1'), eI, sI))) throw new Error('decrypt failed')
        if (expectedRemote && !eq(expectedRemote, sI)) throw new Error('invalid key')
        remoteEph = eI
        remote = sI
        step = 1
        return new Uint8Array(0)
      }
      if (initiator && step === 1 && message[0] === 0x12) {
        const eR = message.slice(1, 9)
        const tag = message.slice(9, 25)
        if (!eq(tag, mac(sha(local.public), text('ik2'), eph, eR)))
          throw new Error('decrypt failed')
        finish(local.public, remote!, eph, eR)
        step = 2
        return new Uint8Array(0)
      }
      throw new Error('malformed frame')
    },
    intoTransport() {
      if (!transport) throw new Error('handshake not finished')
      return transport
    },
    free() {},
  }
}

function normalize(code: string): string {
  return code.replace(/[\s-]/g, '').toUpperCase()
}

/** 规范化并校验：8 或 16 位，只含字母表字符（和 crates/crypto 一致）。 */
function validated(code: string): string {
  const n = normalize(code)
  if ((n.length !== 8 && n.length !== 16) || [...n].some((c) => !ALPHABET.includes(c))) {
    throw new Error('invalid pair code')
  }
  return n
}

const randomCode = (len: number) =>
  Array.from(randomBytes(len), (b) => ALPHABET[b % ALPHABET.length]).join('')
const group = (raw: string) => raw.match(/.{4}/g)!.join('-')

const matchIdOf = (code: string): string =>
  Array.from(sha(text('match'), text(validated(code))).slice(0, 8), (b) =>
    b.toString(16).padStart(2, '0'),
  ).join('')

export const mockCrypto: Crypto = {
  generateKeypair: () => {
    const secret = new Uint8Array(randomBytes(32))
    return { secret, public: publicOf(secret) }
  },
  keypairFromSecret: (secret) => ({ secret: secret.slice(), public: publicOf(secret) }),
  pairing: pairingHandshake,
  reconnect: reconnectHandshake,
  // 玩具实现：不真算 Argon2id（真 wasm 的向量由 packages/protocol/tests/crypto.test.ts 守着）。
  derivePairSecrets: (code) => ({
    matchId: matchIdOf(code),
    psk: sha(text('psk'), text(validated(code))),
  }),
  derivePsk: (code) => sha(text('psk'), text(validated(code))),
  deriveMatchId: matchIdOf,
  normalizeCode: normalize,
  generatePairCode: () => group(randomCode(8)),
  generatePairCodeLong: () => group(randomCode(16)),
  fingerprint: (pub) =>
    Array.from(sha(pub).slice(0, 8), (b) => b.toString(16).padStart(2, '0')).join(':'),
}
