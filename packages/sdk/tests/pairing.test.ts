// 配对（ADR-29）：match_id 撮合、短码不出本机、`pair.ack` 校验后才落盘、不覆盖已固定的节点公钥、
// 吊销 / hub 丢数据后能重新配对。同一组场景对每种 Noise 实现跑一遍。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import { normalizePairCode } from '@anycon/protocol'

import {
  MemoryStorage,
  NodeKeyChanged,
  PairingFailed,
  connect,
  forgetNode,
  pair,
  type StoredIdentity,
} from '../src/index.js'
import { StateStore } from '../src/storage.js'
import { FakeDaemon } from './fake-daemon.js'
import { FAST, cryptoImpls, startEnv, until, type Env } from './harness.js'

/** 失败的配对不留下任何东西。 */
async function assertNothingSaved(storage: MemoryStorage): Promise<void> {
  assert.equal(await storage.get('identity'), undefined, 'no identity saved')
  assert.equal(await storage.get('nodes'), undefined, 'no node saved')
}

/** 用同一个 hub 登记一个新 daemon，返回它（已 start）。 */
async function addDaemon(
  env: Env,
  options: Partial<ConstructorParameters<typeof FakeDaemon>[0]> = {},
): Promise<FakeDaemon> {
  const daemon = new FakeDaemon({
    hubUrl: env.url,
    crypto: env.crypto,
    device: env.hub.registerDevice('daemon'),
    name: 'other-node',
    ...options,
  })
  await daemon.start()
  return daemon
}

for (const impl of cryptoImpls) {
  describe(`配对（${impl.label}）`, () => {
    it('pair() 只调一次 derivePairSecrets（不再分别算 PSK 和 match_id），match_id 与 PSK 来自同一次 KDF', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const calls = { pairSecrets: 0, psk: 0, matchId: 0 }
        const psks: Uint8Array[] = []
        const spy = {
          ...crypto,
          derivePairSecrets: (code: string) => {
            calls.pairSecrets++
            const secrets = crypto.derivePairSecrets(code)
            psks.push(secrets.psk)
            return secrets
          },
          derivePsk: (code: string) => {
            calls.psk++
            return crypto.derivePsk(code)
          },
          deriveMatchId: (code: string) => {
            calls.matchId++
            return crypto.deriveMatchId(code)
          },
        }
        const code = await env.daemon.beginPairing()
        const result = await pair(env.url, {
          code,
          name: 'once',
          storage: env.storage,
          crypto: spy,
        })
        assert.equal(result.nodeId, env.daemon.id)
        assert.deepEqual(calls, { pairSecrets: 1, psk: 0, matchId: 0 })
        // hub 只见到 match_id（16 hex）；PSK 在配对结束后被清零
        const join = env.hub.controlLog.find((c) => c.type === 'pair_join')
        assert.ok(join && join.type === 'pair_join' && /^[0-9a-f]{16}$/.test(join.match_id))
        assert.equal(psks.length, 1)
        assert.ok(
          psks[0]!.every((b) => b === 0),
          'the PSK copy is zeroed after pairing',
        )
      } finally {
        await env.stop()
      }
    })

    it('身份被作废后 pair() 内部重试，Argon2 也只算一次', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const first = await env.pair()
        env.hub.revoke(first.deviceId)
        let derived = 0
        const spy = {
          ...crypto,
          derivePairSecrets: (code: string) => {
            derived++
            return crypto.derivePairSecrets(code)
          },
        }
        const code = await env.daemon.beginPairing()
        const again = await pair(env.url, {
          code,
          name: 'ci-runner',
          storage: env.storage,
          crypto: spy,
        })
        assert.equal(again.nodeId, env.daemon.id)
        assert.equal(derived, 1)
      } finally {
        await env.stop()
      }
    })

    it('8 位短码、不带连字符的小写、16 位长短码、#code= 链接都能配对；hub 只见过 match_id', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        // 8 位，小写、无连字符
        const code8 = await env.daemon.beginPairing()
        assert.match(code8, /^[2-9A-HJKMNP-Z]{4}-[2-9A-HJKMNP-Z]{4}$/)
        const a = await pair(env.url, {
          code: normalizePairCode(code8).toLowerCase(),
          name: 'by-plain-code',
          storage: env.storage,
          crypto,
        })
        assert.equal(a.nodeId, env.daemon.id)

        // 16 位长短码（另一个 storage = 另一台设备）
        const code16 = await env.daemon.beginPairing({ long: true })
        assert.match(code16, /^([2-9A-HJKMNP-Z]{4}-){3}[2-9A-HJKMNP-Z]{4}$/)
        const b = await pair(env.url, {
          code: code16,
          name: 'by-long-code',
          storage: new MemoryStorage(),
          crypto,
        })
        assert.equal(b.nodeId, env.daemon.id)

        // 配对链接：短码在 fragment 里
        const code8b = await env.daemon.beginPairing()
        const viaLink = await pair(env.url, {
          code: `${env.url}/pair#code=${code8b}`,
          name: 'by-link',
          storage: new MemoryStorage(),
          crypto,
        })
        assert.equal(viaLink.nodeId, env.daemon.id)

        // 只有 `#code=…` 的 fragment 也行
        const code16b = await env.daemon.beginPairing({ long: true })
        await pair(env.url, {
          code: `#code=${code16b}`,
          name: 'by-fragment',
          storage: new MemoryStorage(),
          crypto,
        })

        // hub 收到的全部控制消息里没有任何短码，只有 16 hex 的 match_id
        const wire = JSON.stringify(env.hub.controlLog)
        for (const code of [code8, code16, code8b, code16b]) {
          assert.ok(!wire.includes(normalizePairCode(code)), `hub saw ${code}`)
          assert.ok(!wire.includes(code), `hub saw ${code}`)
        }
        const joins = env.hub.controlLog.filter((c) => c.type === 'pair_join')
        assert.equal(joins.length, 4)
        for (const j of joins) {
          assert.ok(j.type === 'pair_join' && /^[0-9a-f]{16}$/.test(j.match_id))
        }
        const begins = env.hub.controlLog.filter((c) => c.type === 'pair_begin')
        assert.deepEqual(
          begins.map((x) => (x.type === 'pair_begin' ? x.match_id : '')),
          joins.map((x) => (x.type === 'pair_join' ? x.match_id : '')),
        )
      } finally {
        await env.stop()
      }
    })

    it('配对结果取自认证过的 pair.ack：节点 id、名字（控制字符被剥掉）、公钥指纹', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto, {
        daemon: { name: 'evil\u001b[2Jname\u0007', pairAck: { name: 'ack\u001b[31m-node\u0000' } },
      })
      try {
        const paired = await env.pair({ name: 'ci-runner' })
        assert.equal(paired.nodeId, env.daemon.id)
        assert.equal(paired.node.name, 'ack[31m-node') // ESC 和 NUL 被去掉，不会把转义序列写进终端
        assert.equal(paired.nodeFingerprint, crypto.fingerprint(env.daemon.keypair.public))
        const nodes = await new StateStore(env.storage).getNodes()
        assert.equal(nodes.length, 1)
        assert.equal(nodes[0]!.id, env.daemon.id)
        assert.equal(nodes[0]!.publicKey.length, 64)
        assert.ok(await env.storage.get('identity'))
      } finally {
        await env.stop()
      }
    })

    it('pair.ack 先于 PairDone 到达：先缓冲，PairDone 到了再校验', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        env.hub.delayJoinerPairDoneMs = 150
        const paired = await env.pair()
        assert.equal(paired.nodeId, env.daemon.id)
        assert.ok(await env.storage.get('identity'))
      } finally {
        await env.stop()
      }
    })

    it('PairDone 之后 hub 立刻发的 PeerOnline 不是配对成功的信号——没有 pair.ack 就是失败', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto, { daemon: { pairAck: false } })
      try {
        const code = await env.daemon.beginPairing()
        await assert.rejects(
          pair(env.url, { code, name: 'x', storage: env.storage, crypto, ackTimeoutMs: 250 }),
          (err: unknown) => err instanceof PairingFailed && err.reason === 'timeout',
        )
        // fake hub 和真实 hub 一样，在 PairDone 之后给双方发了 PeerOnline（包括 joiner）
        const joiner = [...env.hub.devices.values()].find((d) => d.kind === 'sdk')
        assert.ok(joiner)
        assert.deepEqual(
          env.hub.peerOnlineAfterPair.toSorted(),
          [env.daemon.id, joiner.id].toSorted(),
        )
        await assertNothingSaved(env.storage)
      } finally {
        await env.stop()
      }
    })

    it('daemon 不发 pair.ack → 超时失败，什么都不落盘', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto, { daemon: { pairAck: false } })
      try {
        const code = await env.daemon.beginPairing()
        const started = Date.now()
        await assert.rejects(
          pair(env.url, { code, name: 'x', storage: env.storage, crypto, ackTimeoutMs: 250 }),
          (err: unknown) => {
            assert.ok(err instanceof PairingFailed)
            assert.equal(err.reason, 'timeout')
            assert.match(err.message, /pair\.ack/)
            return true
          },
        )
        assert.ok(Date.now() - started >= 200)
        await assertNothingSaved(env.storage)
      } finally {
        await env.stop()
      }
    })

    it('pair.ack 的 node_id / peer_id / fingerprint 任何一项不符 → 失败，什么都不落盘', async () => {
      const crypto = await impl.load()
      const cases: Array<[string, Record<string, string>, RegExp]> = [
        ['node_id', { node_id: 'f'.repeat(32) }, /node_id/],
        ['peer_id', { peer_id: 'e'.repeat(32) }, /peer_id/],
        ['fingerprint', { fingerprint: '00:11:22:33:44:55:66:77' }, /fingerprint/],
      ]
      for (const [label, override, pattern] of cases) {
        const env = await startEnv(crypto, { daemon: { pairAck: override } })
        try {
          const code = await env.daemon.beginPairing()
          await assert.rejects(
            pair(env.url, { code, name: 'x', storage: env.storage, crypto, ackTimeoutMs: 2000 }),
            (err: unknown) => {
              assert.ok(err instanceof PairingFailed, label)
              assert.equal(err.reason, 'protocol', label)
              assert.match(err.message, pattern, label)
              return true
            },
          )
          await assertNothingSaved(env.storage)
        } finally {
          await env.stop()
        }
      }
    })

    it('hub 在 PairDone 里谎报节点 id（pair.ack 说的是真 id）→ 失败，什么都不落盘', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        env.hub.tamperPairDonePeer = () => 'a'.repeat(32)
        const code = await env.daemon.beginPairing()
        await assert.rejects(
          pair(env.url, { code, name: 'x', storage: env.storage, crypto, ackTimeoutMs: 2000 }),
          (err: unknown) => {
            assert.ok(err instanceof PairingFailed)
            assert.match(err.message, /node_id/)
            return true
          },
        )
        await assertNothingSaved(env.storage)
      } finally {
        await env.stop()
      }
    })

    it('中间人 hub 伪造 PairDone（对端没有 PSK，造不出 pair.ack）→ 超时失败，什么都不落盘', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const code = await env.daemon.beginPairing()
        env.daemon.setPairCode('AAAA-BBBB') // 对端握手用的 PSK 和 joiner 的不一样：第 3 条读不开
        env.hub.forgePairDoneOnConfirm = true // joiner 一发 pair_confirm，hub 就伪造 PairDone
        await assert.rejects(
          pair(env.url, { code, name: 'x', storage: env.storage, crypto, ackTimeoutMs: 300 }),
          (err: unknown) => {
            assert.ok(err instanceof PairingFailed)
            assert.equal(err.reason, 'timeout')
            return true
          },
        )
        await assertNothingSaved(env.storage)
        // 对端没有因此认识这台设备
        assert.equal(env.daemon.peers.size, 0)
      } finally {
        await env.stop()
      }
    })

    it('第一条消息不是 pair.ack（或解不开）→ 失败，什么都不落盘', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto, { daemon: { pairAck: false } })
      try {
        const code = await env.daemon.beginPairing()
        const waiting = pair(env.url, {
          code,
          name: 'x',
          storage: env.storage,
          crypto,
          ackTimeoutMs: 3000,
        })
        // PairDone 之后，hub 往 joiner 塞一封伪造的 data（解不开）
        await until(
          () => env.hub.controlLog.some((c) => c.type === 'pair_confirm'),
          2000,
          'joiner confirm',
        )
        await new Promise((r) => setTimeout(r, 100))
        const joiner = [...env.hub.devices.values()].find((d) => d.kind === 'sdk')
        assert.ok(joiner)
        env.daemon.sendGarbage(joiner.id)
        await assert.rejects(waiting, (err: unknown) => {
          assert.ok(err instanceof PairingFailed)
          assert.equal(err.reason, 'handshake')
          return true
        })
        await assertNothingSaved(env.storage)
      } finally {
        await env.stop()
      }
    })

    it('不覆盖已固定的节点公钥：同 id 换了公钥 → NodeKeyChanged，旧记录原样保留；forgetNode 之后可以重新配对', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const first = await env.pair({ name: 'ci-runner' })
        const store = new StateStore(env.storage)
        const before = (await store.getNodes())[0]!
        const identityBefore = await env.storage.get('identity')

        // 同一个设备 id 的 daemon 换了密钥（重装 / 被冒充）
        env.daemon.stop()
        await until(() => !env.hub.isOnline(env.daemon.id), 2000, 'old daemon to leave')
        const device = [...env.hub.devices.values()].find((d) => d.id === env.daemon.id)!
        const impostor = new FakeDaemon({
          hubUrl: env.url,
          crypto,
          device: { id: device.id, token: device.token },
          name: 'impostor',
        })
        await impostor.start()
        try {
          const code = await impostor.beginPairing()
          await assert.rejects(
            pair(env.url, { code, name: 'ci-runner', storage: env.storage, crypto }),
            (err: unknown) => {
              assert.ok(err instanceof NodeKeyChanged)
              assert.equal(err.nodeId, env.daemon.id)
              assert.equal(err.storedFingerprint, before.fingerprint)
              assert.equal(err.presentedFingerprint, crypto.fingerprint(impostor.keypair.public))
              assert.match(err.message, /forgetNode/)
              return true
            },
          )
          // 旧记录没动，身份也没被换
          assert.deepEqual((await store.getNodes())[0], before)
          assert.equal(await env.storage.get('identity'), identityBefore)

          // 用户确认新公钥可信：忘掉旧记录再配对
          const forgotten = await forgetNode(env.url, first.nodeId.slice(0, 6), {
            storage: env.storage,
          })
          assert.equal(forgotten.id, first.nodeId)
          assert.equal((await store.getNodes()).length, 0)
          const code2 = await impostor.beginPairing()
          const again = await pair(env.url, {
            code: code2,
            name: 'ci-runner',
            storage: env.storage,
            crypto,
          })
          assert.equal(again.nodeId, env.daemon.id)
          assert.equal(
            (await store.getNodes())[0]!.fingerprint,
            crypto.fingerprint(impostor.keypair.public),
          )
        } finally {
          impostor.stop()
        }
      } finally {
        await env.stop()
      }
    })

    it('同 id 同公钥（吊销后重新配对）允许，记录被刷新', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const first = await env.pair({ name: 'ci-runner' })
        const store = new StateStore(env.storage)
        const second = await env.pair({ name: 'ci-runner' })
        assert.equal(second.nodeId, first.nodeId)
        assert.equal(second.deviceId, first.deviceId) // 已有身份，沿用
        const nodes = await store.getNodes()
        assert.equal(nodes.length, 1)
        assert.ok(nodes[0]!.pairedAtMs >= first.node.pairedAtMs)
      } finally {
        await env.stop()
      }
    })

    it('设备被 hub 吊销后 pair() 清掉作废的身份并重新配对成功', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const first = await env.pair({ name: 'ci-runner' })
        env.hub.revoke(first.deviceId)
        // 没有先 connect()（它会顺手清凭据）：pair() 自己要能从「身份已作废」里恢复
        assert.ok(await env.storage.get('identity'))
        const again = await env.pair({ name: 'ci-runner' })
        assert.notEqual(again.deviceId, first.deviceId)
        const identity = JSON.parse((await env.storage.get('identity'))!) as StoredIdentity
        assert.equal(identity.deviceId, again.deviceId)
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          assert.equal((await node.info()).node_id, env.daemon.id)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('hub 不认本地 token（hub 丢了数据）时 pair() 也能重新配对', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const first = await env.pair({ name: 'ci-runner' })
        const stored = JSON.parse((await env.storage.get('identity'))!) as StoredIdentity
        stored.deviceToken = 'tok_unknown_to_the_hub'
        await env.storage.set('identity', JSON.stringify(stored))
        const again = await env.pair({ name: 'ci-runner' })
        assert.notEqual(again.deviceId, first.deviceId)
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        await node.close()
      } finally {
        await env.stop()
      }
    })

    it('不是合法短码或链接：立刻失败，不连 hub', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        for (const bad of [
          '',
          '7K3P-Q2M',
          '7K3P-Q2M0',
          `${env.url}/pair?code=7K3P-Q2MX`, // query 里的不认
          `${env.url}/pair`,
          '7K3P-Q2MX-9ACD', // 12 位
        ]) {
          await assert.rejects(
            pair(env.url, { code: bad, name: 'x', storage: env.storage, crypto }),
            PairingFailed,
            JSON.stringify(bad),
          )
        }
        assert.equal(env.hub.authCount.size, 1) // 只有 daemon 自己连过
        await assertNothingSaved(env.storage)
      } finally {
        await env.stop()
      }
    })

    it('hub 看到的 pair_join 只有 match_id；两个 daemon 的短码互不串', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      const second = await addDaemon(env)
      try {
        const codeA = await env.daemon.beginPairing()
        const codeB = await second.beginPairing()
        const b = await pair(env.url, {
          code: codeB,
          name: 'b',
          storage: new MemoryStorage(),
          crypto,
        })
        const a = await pair(env.url, {
          code: codeA,
          name: 'a',
          storage: new MemoryStorage(),
          crypto,
        })
        assert.equal(a.nodeId, env.daemon.id)
        assert.equal(b.nodeId, second.id)
      } finally {
        second.stop()
        await env.stop()
      }
    })
  })
}
