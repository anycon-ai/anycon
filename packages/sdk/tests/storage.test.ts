import assert from 'node:assert/strict'
import { mkdtemp, readFile, rm, stat } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { describe, it } from 'node:test'

import { IDBFactory } from 'fake-indexeddb'

import {
  AnyconError,
  FileStorage,
  IndexedDBStorage,
  MemoryStorage,
  assertSafeProfile,
  connect,
  defaultSdkDir,
  defaultStorage,
  hubHostKey,
  resolveProfile,
  type SdkStorage,
} from '../src/index.js'
import { profileDir } from '../src/storage-file.js'
import { StateStore, envCredentials } from '../src/storage.js'
import { FAST, cryptoImpls, startEnv } from './harness.js'

async function exercise(storage: SdkStorage): Promise<void> {
  assert.equal(await storage.get('a'), undefined)
  await storage.set('a', '1')
  await storage.set('b', '{"x":"中文"}')
  assert.equal(await storage.get('a'), '1')
  assert.equal(await storage.get('b'), '{"x":"中文"}')
  await storage.set('a', '2')
  assert.equal(await storage.get('a'), '2')
  await storage.delete('a')
  await storage.delete('a') // 幂等
  assert.equal(await storage.get('a'), undefined)
  assert.equal(await storage.get('b'), '{"x":"中文"}')
}

describe('存储后端', () => {
  it('MemoryStorage', async () => {
    await exercise(new MemoryStorage())
  })

  it('FileStorage：目录 0700、文件 0600、原子写、拒绝非法键', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'anycon-sdk-test-'))
    try {
      const storage = new FileStorage(join(dir, 'hub.example.com', 'default'))
      await exercise(storage)
      await storage.set('identity', '{"secret":"x"}')
      const file = join(dir, 'hub.example.com', 'default', 'identity.json')
      assert.equal((await stat(file)).mode & 0o777, 0o600)
      assert.equal((await stat(join(dir, 'hub.example.com', 'default'))).mode & 0o777, 0o700)
      assert.equal(await readFile(file, 'utf8'), '{"secret":"x"}')
      await assert.rejects(storage.get('../escape'), /invalid storage key/)
      await assert.rejects(storage.set('a/b', 'x'), /invalid storage key/)
    } finally {
      await rm(dir, { recursive: true, force: true })
    }
  })

  it('IndexedDBStorage（fake-indexeddb）：持久化在同一数据库内，不同 scope 隔离', async () => {
    const factory = new IDBFactory()
    const a = new IndexedDBStorage('hub-a/default', factory)
    await exercise(a)
    await a.set('identity', 'persisted')
    await a.close()
    const again = new IndexedDBStorage('hub-a/default', factory)
    assert.equal(await again.get('identity'), 'persisted') // 重新打开仍在
    const other = new IndexedDBStorage('hub-b/default', factory)
    assert.equal(await other.get('identity'), undefined)
    await again.close()
    await other.close()
  })

  it('StateStore：节点增删改、并发写不丢', async () => {
    const store = new StateStore(new MemoryStorage())
    await Promise.all(
      Array.from({ length: 20 }, (_, i) =>
        store.putNode({ id: `n${i}`, publicKey: '00', pairedAtMs: i }),
      ),
    )
    assert.equal((await store.getNodes()).length, 20)
    await store.updateNode('n3', { name: 'three' })
    await store.updateNode('missing', { name: 'x' }) // 忽略
    await store.removeNode('n4')
    const nodes = await store.getNodes()
    assert.equal(nodes.length, 19)
    assert.equal(nodes.find((n) => n.id === 'n3')?.name, 'three')
  })

  it('损坏的存储内容给出明确错误，而不是静默覆盖', async () => {
    const storage = new MemoryStorage()
    await storage.set('identity', '{not json')
    await assert.rejects(new StateStore(storage).getIdentity(), /not valid JSON/)
  })

  it('profile 不能带路径分隔符 / `..` / 空串，拒绝而不是改写', () => {
    for (const ok of ['default', 'ci', 'ci-1.2', 'A_b', 'a'.repeat(64)]) {
      assert.equal(assertSafeProfile(ok), ok)
      assert.equal(resolveProfile(ok, {}), ok)
    }
    for (const bad of [
      '',
      '.',
      '..',
      '../x',
      'a/b',
      'a\\b',
      '..\\x',
      '/etc',
      'a..b',
      '.hidden',
      '-x',
      'a b',
      'a\0b',
      'a'.repeat(65),
    ]) {
      assert.throws(
        () => assertSafeProfile(bad),
        (err: unknown) => err instanceof AnyconError && err.code === 'invalid_profile',
        JSON.stringify(bad),
      )
      assert.throws(() => resolveProfile(bad, {}), /invalid profile/, JSON.stringify(bad))
    }
    // 环境变量里的 profile 同样要过检查；空串当作没设
    assert.throws(() => resolveProfile(undefined, { ANYCON_PROFILE: '../../x' }), /invalid profile/)
    assert.equal(resolveProfile(undefined, { ANYCON_PROFILE: '' }), 'default')
  })

  it('profileDir 一定落在 sdk 目录之内；hub 主机名里的 `..` 之类也不能跑出去', () => {
    assert.equal(
      profileDir('/home/u/.anycon/sdk', 'https://hub.example.com:8443', 'ci'),
      '/home/u/.anycon/sdk/hub.example.com_8443/ci',
    )
    assert.equal(
      profileDir('/home/u/.anycon/sdk/', 'https://hub.example.com', undefined),
      '/home/u/.anycon/sdk/hub.example.com/default',
    )
    assert.throws(
      () => profileDir('/x/sdk', 'https://hub.example.com', '../../etc'),
      /invalid profile/,
    )
    assert.throws(() => hubHostKey('https://..'), /cannot derive a storage name/)
  })

  it('ANYCON_HOME 是空串时当作没设，而不是变成根目录下的 /sdk', async () => {
    const saved = process.env.ANYCON_HOME
    try {
      process.env.ANYCON_HOME = ''
      const dir = await defaultSdkDir()
      assert.notEqual(dir, '/sdk')
      assert.match(dir, /\/\.anycon\/sdk$/)
      process.env.ANYCON_HOME = '/custom/home/'
      assert.equal(await defaultSdkDir(), '/custom/home/sdk')
    } finally {
      if (saved === undefined) delete process.env.ANYCON_HOME
      else process.env.ANYCON_HOME = saved
    }
  })

  it('defaultStorage 对非法 profile 直接抛错，不创建任何目录', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'anycon-sdk-test-'))
    const saved = process.env.ANYCON_HOME
    try {
      process.env.ANYCON_HOME = dir
      await assert.rejects(
        defaultStorage('https://hub.example.com', '../../evil'),
        /invalid profile/,
      )
      await assert.rejects(stat(join(dir, 'sdk')), { code: 'ENOENT' })
      const ok = await defaultStorage('https://hub.example.com', 'ci')
      assert.ok(ok instanceof FileStorage)
      assert.equal(ok.dir, join(dir, 'sdk', 'hub.example.com', 'ci'))
    } finally {
      if (saved === undefined) delete process.env.ANYCON_HOME
      else process.env.ANYCON_HOME = saved
      await rm(dir, { recursive: true, force: true })
    }
  })

  it('辅助：hub 主机键、profile、环境变量凭据', () => {
    assert.equal(hubHostKey('https://hub.example.com:8443/x'), 'hub.example.com_8443')
    assert.equal(hubHostKey('hub.example.com'), 'hub.example.com')
    assert.equal(resolveProfile(undefined, {}), 'default')
    assert.equal(resolveProfile(undefined, { ANYCON_PROFILE: 'ci' }), 'ci')
    assert.equal(resolveProfile('explicit', { ANYCON_PROFILE: 'ci' }), 'explicit')
    assert.deepEqual(envCredentials({}), {})
    assert.deepEqual(envCredentials({ ANYCON_DEVICE_KEY: 'k' }), {}) // 只给 key 不够
    const env = envCredentials({
      ANYCON_DEVICE_KEY: 'kk',
      ANYCON_DEVICE_TOKEN: 'tt',
      ANYCON_NODE_ID: 'nid',
      ANYCON_NODE_KEY: 'pk',
      ANYCON_NODE_NAME: 'box',
    })
    assert.equal(env.identity?.deviceToken, 'tt')
    assert.equal(env.node?.name, 'box')
  })
})

for (const impl of cryptoImpls) {
  describe(`持久化与注入（${impl.label}）`, () => {
    it('FileStorage 里配对一次，换一个 connect 进程（新 storage 对象）仍可连接', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      const dir = await mkdtemp(join(tmpdir(), 'anycon-sdk-test-'))
      try {
        const code = await env.daemon.beginPairing()
        const { pair } = await import('../src/index.js')
        const result = await pair(env.url, {
          code,
          name: 'persisted',
          storage: new FileStorage(dir),
          crypto,
        })
        const node = await connect(env.url, {
          storage: new FileStorage(dir),
          crypto,
          node: result.nodeName,
          ...FAST,
        })
        try {
          assert.equal((await node.info()).node_id, env.daemon.id)
        } finally {
          await node.close()
        }
      } finally {
        await rm(dir, { recursive: true, force: true })
        await env.stop()
      }
    })

    it('ANYCON_DEVICE_KEY / ANYCON_DEVICE_TOKEN / ANYCON_NODE_* 注入凭据，不落盘', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const identity = JSON.parse((await env.storage.get('identity'))!) as {
          secretKey: string
          deviceToken: string
        }
        const nodes = JSON.parse((await env.storage.get('nodes'))!) as Array<{
          id: string
          publicKey: string
        }>
        const saved = { ...process.env }
        Object.assign(process.env, {
          ANYCON_DEVICE_KEY: identity.secretKey,
          ANYCON_DEVICE_TOKEN: identity.deviceToken,
          ANYCON_NODE_ID: nodes[0]!.id,
          ANYCON_NODE_KEY: nodes[0]!.publicKey,
        })
        try {
          const empty = new MemoryStorage()
          const node = await connect(env.url, { storage: empty, crypto, ...FAST })
          try {
            assert.equal((await node.info()).node_id, env.daemon.id)
          } finally {
            await node.close()
          }
          assert.equal(await empty.get('identity'), undefined)
        } finally {
          for (const key of [
            'ANYCON_DEVICE_KEY',
            'ANYCON_DEVICE_TOKEN',
            'ANYCON_NODE_ID',
            'ANYCON_NODE_KEY',
          ]) {
            if (key in saved) process.env[key] = saved[key]
            else delete process.env[key]
          }
        }
      } finally {
        await env.stop()
      }
    })
  })
}
