// NodeLink 的握手编排：IK 握手在途时又收到 peer_online 怎么办（重新配对后 attachNode 和 hub 各触发一次，相隔几毫秒）。
// 回复只能配给发出它的那次握手——假 crypto 把握手临时密钥回显在回复里，配错就会抛错（和真实 Noise 一样）。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createFakeCrypto } from '@/test/fakeCrypto'
import { DUPLICATE_PEER_ONLINE_MS, HANDSHAKE_TIMEOUT_MS } from '@/lib/constants'
import type { Handshake } from './crypto'
import { NodeLink, type LinkContext } from './nodeLink'

const crypto = createFakeCrypto()
const daemonKp = crypto.generateKeypair()
const clientKp = crypto.generateKeypair()

function setup(publicKey: Uint8Array = daemonKp.public) {
  const sent: Uint8Array[] = []
  const ready = vi.fn()
  const changes: string[] = []
  let canSend = true
  let held = false
  let sendFails = false
  const ctx: LinkContext = {
    crypto,
    keypair: clientKp,
    canSend: () => canSend,
    sendData: () => 1,
    discardPending: () => {},
    handshakeHeld: () => held,
    sendHandshake: (_to, msg) => {
      if (sendFails) return null
      sent.push(msg)
      return sent.length
    },
    onChange: () => {},
    onReady: ready,
    onNotification: () => {},
  }
  const link = new NodeLink('d'.repeat(32), publicKey, ctx)
  return {
    link,
    sent,
    ready,
    changes,
    setCanSend: (v: boolean) => (canSend = v),
    /** 发送方扣着握手信封（hub 退回后延后重发中 / 在等 rate_limited）。 */
    setHeld: (v: boolean) => (held = v),
    setSendFails: (v: boolean) => (sendFails = v),
    /** daemon 读第 n 条 IK 消息并产生回复（不投递）。 */
    reply(n: number): Uint8Array {
      const hs: Handshake = crypto.reconnect('responder', daemonKp)
      hs.read(sent[n]!)
      const out = hs.write()
      hs.intoTransport()
      return out
    },
  }
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] })
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => vi.useRealTimers())

describe('NodeLink handshake orchestration', () => {
  it('two peerOnline() back to back send a single IK, and its reply completes it', () => {
    const t = setup()
    t.link.peerOnline() // attachNode
    t.link.peerOnline() // hub 的 PeerOnline，紧随其后
    expect(t.sent).toHaveLength(1)
    t.link.onHandshake(t.reply(0))
    expect(t.link.ready).toBe(true)
    expect(t.link.machine.noise).toBe('ready')
    expect(t.ready).toHaveBeenCalledTimes(1)
    expect(t.sent).toHaveLength(1) // 之后也没有补发
  })

  it('a duplicate that arrives a little later (still inside the window) is ignored as well', () => {
    const t = setup()
    t.link.peerOnline()
    vi.advanceTimersByTime(DUPLICATE_PEER_ONLINE_MS - 1)
    t.link.peerOnline()
    expect(t.sent).toHaveLength(1)
    t.link.onHandshake(t.reply(0))
    expect(t.link.ready).toBe(true)
  })

  it('a later peerOnline() (the node really reconnected) never runs two handshakes in parallel: the old reply is discarded and one fresh IK follows', () => {
    const t = setup()
    t.link.peerOnline()
    vi.advanceTimersByTime(DUPLICATE_PEER_ONLINE_MS + 1)
    t.link.peerOnline()
    expect(t.sent).toHaveLength(1) // 还没有第二条
    t.link.onHandshake(t.reply(0)) // 第一条的回复到了：丢弃（对端可能已经换了连接），重新握手
    expect(t.link.ready).toBe(false)
    expect(t.ready).not.toHaveBeenCalled()
    expect(t.sent).toHaveLength(2)
    t.link.onHandshake(t.reply(1))
    expect(t.link.ready).toBe(true)
    expect(t.ready).toHaveBeenCalledTimes(1)
  })

  it('if the in-flight handshake times out after a later peerOnline(), it restarts as a fresh attempt instead of counting as a failure', () => {
    const t = setup()
    t.link.peerOnline()
    vi.advanceTimersByTime(DUPLICATE_PEER_ONLINE_MS + 1)
    t.link.peerOnline()
    vi.advanceTimersByTime(HANDSHAKE_TIMEOUT_MS)
    expect(t.sent).toHaveLength(2)
    expect(t.link.machine).toMatchObject({ noise: 'handshaking', attempts: 0 })
    t.link.onHandshake(t.reply(1))
    expect(t.link.ready).toBe(true)
  })

  it('a reply that cannot be read is not a failure when a newer peerOnline() already asked for a restart', () => {
    const t = setup()
    t.link.peerOnline()
    vi.advanceTimersByTime(DUPLICATE_PEER_ONLINE_MS + 1)
    t.link.peerOnline()
    t.link.onHandshake(Uint8Array.of(0x12, 1, 2, 3)) // 垃圾
    expect(t.link.machine.noise).toBe('handshaking')
    expect(t.sent).toHaveLength(2)
  })

  it('a peerOnline() while ready still re-handshakes immediately (the daemon reconnected)', () => {
    const t = setup()
    t.link.peerOnline()
    t.link.onHandshake(t.reply(0))
    expect(t.link.ready).toBe(true)
    t.link.peerOnline()
    expect(t.link.ready).toBe(false)
    expect(t.sent).toHaveLength(2)
    t.link.onHandshake(t.reply(1))
    expect(t.link.ready).toBe(true)
  })

  it('peerOffline() cancels a pending restart', () => {
    const t = setup()
    t.link.peerOnline()
    vi.advanceTimersByTime(DUPLICATE_PEER_ONLINE_MS + 1)
    t.link.peerOnline()
    t.link.peerOffline()
    t.link.peerOnline()
    expect(t.sent).toHaveLength(2) // 新一轮正常开始
    t.link.onHandshake(t.reply(1))
    expect(t.link.ready).toBe(true)
    expect(t.sent).toHaveLength(2)
  })

  describe('rekey (re-pairing a known node)', () => {
    it('with the same key while a handshake is in flight does nothing: that handshake already uses the key', () => {
      const t = setup()
      t.link.peerOnline() // hub 的 PeerOnline 先到
      t.link.rekey(daemonKp.public) // 然后 pair.ack 校验完，attachNode → rekey
      expect(t.sent).toHaveLength(1)
      t.link.onHandshake(t.reply(0))
      expect(t.link.ready).toBe(true)
    })

    it('with a different key restarts once the in-flight handshake has resolved (never in parallel)', () => {
      const t = setup()
      t.link.peerOnline()
      const other = crypto.generateKeypair()
      vi.advanceTimersByTime(DUPLICATE_PEER_ONLINE_MS + 1)
      t.link.rekey(other.public)
      expect(t.sent).toHaveLength(1)
      t.link.onHandshake(Uint8Array.of(0x12, 9, 9, 9)) // 旧钥匙的握手失败 / 作废
      expect(t.sent).toHaveLength(2)
      // 第二条是用新钥匙发的：只有持有新钥匙的 daemon 才读得出来
      expect(() => crypto.reconnect('responder', other).read(t.sent[1]!)).not.toThrow()
      expect(() => crypto.reconnect('responder', daemonKp).read(t.sent[1]!)).toThrow()
      expect(() => crypto.reconnect('responder', daemonKp).read(t.sent[0]!)).not.toThrow()
    })

    it('with the same key on a ready link re-handshakes (the previous behaviour)', () => {
      const t = setup()
      t.link.peerOnline()
      t.link.onHandshake(t.reply(0))
      t.link.rekey(daemonKp.public)
      expect(t.sent).toHaveLength(2)
    })
  })

  describe('handshake returned by the hub', () => {
    it('resendHandshake() sends the very same first message again, and the daemon only ever saw one of them, so the reply completes it', () => {
      const t = setup()
      t.link.peerOnline()
      t.setHeld(true) // hub 退回了它，发送方在延后
      t.setHeld(false) // 到点了
      t.link.resendHandshake()
      expect(t.sent).toHaveLength(2)
      expect(Array.from(t.sent[1]!)).toEqual(Array.from(t.sent[0]!))
      t.link.onHandshake(t.reply(1))
      expect(t.link.ready).toBe(true)
      expect(t.link.machine.attempts).toBe(0)
      expect(t.ready).toHaveBeenCalledTimes(1)
    })

    it('does not count the handshake timeout while the sender is holding the handshake back (no false "daemon does not know us")', () => {
      const t = setup()
      t.link.peerOnline()
      t.setHeld(true)
      vi.advanceTimersByTime(HANDSHAKE_TIMEOUT_MS * 5) // 被限速扣了 40 s
      expect(t.link.machine).toMatchObject({ noise: 'handshaking', attempts: 0 })
      expect(t.sent).toHaveLength(1) // 也没有自己补发
      t.setHeld(false)
      t.link.resendHandshake()
      t.link.onHandshake(t.reply(1))
      expect(t.link.ready).toBe(true)
    })

    it('the timeout counts again from the moment the handshake is actually resent', () => {
      const t = setup()
      t.link.peerOnline()
      t.setHeld(true)
      vi.advanceTimersByTime(HANDSHAKE_TIMEOUT_MS * 2)
      t.setHeld(false)
      t.link.resendHandshake()
      vi.advanceTimersByTime(HANDSHAKE_TIMEOUT_MS - 1)
      expect(t.link.machine.attempts).toBe(0)
      vi.advanceTimersByTime(2) // 重发之后 8 s 没有回复：这才是对端没回
      expect(t.link.machine.attempts).toBe(1)
    })

    it('a peerOnline() while the handshake is held back needs no restart afterwards: nothing has reached the daemon yet', () => {
      const t = setup()
      t.link.peerOnline()
      vi.advanceTimersByTime(DUPLICATE_PEER_ONLINE_MS + 1)
      t.setHeld(true)
      t.link.peerOnline() // 对端换了连接
      expect(t.sent).toHaveLength(1)
      t.setHeld(false)
      t.link.resendHandshake()
      t.link.onHandshake(t.reply(1))
      expect(t.link.ready).toBe(true) // 没有「回复可疑、丢掉重来」
      expect(t.sent).toHaveLength(2)
    })

    it('is a no-op once the handshake is over or abandoned', () => {
      const t = setup()
      t.link.resendHandshake() // 还没开始
      expect(t.sent).toHaveLength(0)
      t.link.peerOnline()
      t.link.onHandshake(t.reply(0))
      t.link.resendHandshake() // 已经完成
      expect(t.sent).toHaveLength(1)
      t.link.peerOffline()
      t.link.resendHandshake() // 对端下线
      expect(t.sent).toHaveLength(1)
      t.link.peerOnline()
      t.setCanSend(false)
      t.link.resendHandshake() // hub 眼下不可写
      expect(t.sent).toHaveLength(2)
    })

    it('gives up the round (back to "none") if the resend cannot be written', () => {
      const t = setup()
      t.link.peerOnline()
      t.setSendFails(true)
      t.link.resendHandshake()
      expect(t.link.machine.noise).toBe('none')
    })
  })
})
