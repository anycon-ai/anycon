// ADR-35：附件。覆盖 node.uploadImage（分块、在途上限、重试、排队、中止、mime 判断）、node.uploadFile（任意文件、
// mime 猜测与校验、和图片共用队列）、downloadAttachment / discardAttachment、session.send / sessions.create 带附件，
// acceptsAttachment（含通配 `ATTACHMENT_ANY_FILE`），以及 daemon 的拒绝映射成带 reason 的 AttachmentError。
// fake daemon 的 blob.* 见 tests/fake-daemon.ts。
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { describe, it } from 'node:test'

import { AnyconRpcError, type Crypto } from '@anycon/protocol'

import {
  ATTACHMENT_ANY_FILE,
  AnyconError,
  AttachmentError,
  RpcError,
  acceptsAttachment,
  connect,
  detectImageMime,
  guessMime,
  normalizeMime,
  type Node,
  type StreamEvent,
} from '../src/index.js'
import { mapRpcError } from '../src/errors.js'
import type { Env } from './harness.js'
import { FAST, collect, cryptoImpls, sleep, startEnv, until } from './harness.js'

const KIB = 1024
const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

/** 和 daemon 的 `testutil::png_fixture` 一样：PNG 签名 + IHDR（宽高）+ 伪随机填充到 `total` 字节。 */
function png(width: number, height: number, total: number, seed = 1): Uint8Array {
  const head = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52]
  const out = new Uint8Array(total)
  out.set(head)
  const v = new DataView(out.buffer)
  v.setUint32(16, width)
  v.setUint32(20, height)
  out.set([8, 6, 0, 0, 0], 24)
  let x = seed + 1
  for (let i = 29; i < total; i++) {
    x = (Math.imul(x, 1_664_525) + 1_013_904_223) >>> 0
    out[i] = x >>> 24
  }
  return out
}

const jpeg = (total: number) => {
  const out = png(1, 1, total)
  out.set([0xff, 0xd8, 0xff, 0xe0], 0)
  return out
}

const ascii = (s: string) => Uint8Array.from(s, (c) => c.charCodeAt(0))
const sha256 = (b: Uint8Array) => createHash('sha256').update(b).digest('hex')
const decode = (data: string) => new Uint8Array(Buffer.from(data, 'base64'))

async function withNode(
  crypto: Crypto,
  fn: (node: Node, env: Env) => Promise<void>,
): Promise<void> {
  const env = await startEnv(crypto)
  try {
    await env.pair()
    const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
    try {
      await fn(node, env)
    } finally {
      await node.close()
    }
  } finally {
    await env.stop()
  }
}

const paramsOf = (env: Env, method: string): Array<Record<string, unknown>> =>
  env.daemon.requests
    .filter((r) => r.method === method)
    .map((r) => r.params as Record<string, unknown>)

const blobMethods = (env: Env) =>
  env.daemon.requests.filter((r) => r.method.startsWith('blob.')).map((r) => r.method)

/** 断言 promise 以 `AttachmentError` 失败，且稳定码 / reason 符合。 */
async function rejectsAttachment(
  p: Promise<unknown>,
  code: string,
  reason: string,
): Promise<AttachmentError> {
  try {
    await p
  } catch (err) {
    assert.ok(err instanceof AttachmentError, `expected AttachmentError, got ${String(err)}`)
    assert.ok(err instanceof RpcError, 'an AttachmentError is still an RpcError')
    assert.equal(err.code, code)
    assert.equal(err.reason, reason)
    return err
  }
  assert.fail('expected the call to fail')
}

describe('detectImageMime', () => {
  it('和 daemon 一样只看文件头前缀', () => {
    assert.equal(detectImageMime(png(1, 1, 64)), 'image/png')
    assert.equal(detectImageMime(Uint8Array.of(0xff, 0xd8, 0xff, 0xe1)), 'image/jpeg')
    assert.equal(detectImageMime(ascii('GIF87a\x01\x00')), 'image/gif')
    assert.equal(detectImageMime(ascii('GIF89a\x01\x00')), 'image/gif')
    assert.equal(detectImageMime(ascii('RIFF\x00\x00\x00\x00WEBPVP8 ')), 'image/webp')
    // RIFF 但不是 WebP（WAV）、SVG、空、截断的 PNG 签名
    assert.equal(detectImageMime(ascii('RIFF\x00\x00\x00\x00WAVEfmt ')), undefined)
    assert.equal(detectImageMime(ascii('<svg xmlns=')), undefined)
    assert.equal(detectImageMime(new Uint8Array()), undefined)
    assert.equal(detectImageMime(png(1, 1, 64).subarray(0, 4)), undefined)
  })
})

describe('guessMime / normalizeMime / acceptsAttachment', () => {
  it('guessMime：先按文件头认四种图片，再按扩展名查表，都不行是 application/octet-stream', () => {
    const text = new TextEncoder().encode('hello')
    // 文件头优先于扩展名
    assert.equal(guessMime(png(1, 1, 64), 'shot.bin'), 'image/png')
    assert.equal(guessMime(Uint8Array.of(0xff, 0xd8, 0xff, 0xe0), 'photo'), 'image/jpeg')
    assert.equal(guessMime(text, 'report.pdf'), 'application/pdf')
    assert.equal(guessMime(text, 'NOTES.MD'), 'text/markdown')
    assert.equal(guessMime(text, 'data.csv'), 'text/csv')
    assert.equal(guessMime(text, 'a.json'), 'application/json')
    assert.equal(guessMime(text, 'conf.yml'), 'application/yaml')
    assert.equal(guessMime(text, 'conf.yaml'), 'application/yaml')
    assert.equal(guessMime(text, 'src.tar'), 'application/x-tar')
    assert.equal(guessMime(text, 'src.tar.gz'), 'application/gzip')
    assert.equal(guessMime(text, 'main.rs'), 'text/x-rust')
    assert.equal(guessMime(text, 'main.py'), 'text/x-python')
    assert.equal(guessMime(text, 'index.ts'), 'text/x-typescript')
    // 不认识的扩展名、没有扩展名、点开头的文件名
    assert.equal(guessMime(text, 'blob.xyz'), 'application/octet-stream')
    assert.equal(guessMime(text, 'Makefile'), 'application/octet-stream')
    assert.equal(guessMime(text, '.json'), 'application/octet-stream')
    // 扩展名是图片但内容不是：不猜成图片（否则 put_end 会 mime_mismatch）
    assert.equal(guessMime(text, 'fake.png'), 'application/octet-stream')
  })

  it('normalizeMime：成形的转小写，不成形的 undefined', () => {
    assert.equal(normalizeMime('Application/PDF'), 'application/pdf')
    assert.equal(
      normalizeMime('application/vnd.openxmlformats-officedocument.wordprocessingml.document'),
      'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
    )
    assert.equal(normalizeMime('image/svg+xml'), 'image/svg+xml')
    for (const bad of [
      '',
      'pdf',
      'text/',
      '/plain',
      'text/plain; charset=utf-8',
      'text/plain/x',
      ' text/plain',
      '*/*',
      'text/*',
      `application/${'x'.repeat(120)}`,
    ]) {
      assert.equal(normalizeMime(bad), undefined, JSON.stringify(bad))
    }
  })

  it('acceptsAttachment：点名的类型，或者通配 */*；没有 capabilities / 空列表不收', () => {
    const images = { capabilities: { attachments: ['image/png', 'image/jpeg'] } }
    assert.equal(acceptsAttachment(images, 'image/png'), true)
    assert.equal(acceptsAttachment(images, 'IMAGE/JPEG'), true)
    assert.equal(acceptsAttachment(images, 'application/pdf'), false)
    const any = { capabilities: { attachments: ['image/png', ATTACHMENT_ANY_FILE] } }
    assert.equal(ATTACHMENT_ANY_FILE, '*/*')
    assert.equal(acceptsAttachment(any, 'application/pdf'), true)
    assert.equal(acceptsAttachment(any, 'application/octet-stream'), true)
    assert.equal(acceptsAttachment({}, 'image/png'), false)
    assert.equal(acceptsAttachment({ capabilities: {} }, 'image/png'), false)
    assert.equal(acceptsAttachment({ capabilities: { attachments: [] } }, 'image/png'), false)
  })
})

describe('mapRpcError：附件的拒绝', () => {
  it('blob 通道的 reason 映射成 AttachmentError，别的 reason 和码不动', () => {
    const cases: Array<[Parameters<typeof AnyconRpcError.of>[0], string]> = [
      ['invalid', 'too_large'],
      ['busy', 'quota'],
      ['busy', 'disk_full'],
      ['not_found', 'blob'],
      ['invalid', 'blob_in_use'],
      ['invalid', 'in_use'],
      ['invalid', 'finalizing'],
      ['invalid', 'incomplete'],
      ['invalid', 'hash_mismatch'],
      ['invalid', 'mime_mismatch'],
    ]
    for (const [code, reason] of cases) {
      const err = mapRpcError(AnyconRpcError.of(code, 'x', { reason }))
      assert.ok(err instanceof AttachmentError, `${code}/${reason}`)
      assert.equal(err.code, code)
      assert.equal(err.reason, reason)
    }
    // 别的功能的 reason、没有 reason 的 busy、码对不上的：仍是普通 RpcError
    for (const err of [
      mapRpcError(AnyconRpcError.of('invalid', 'x', { reason: 'archived' })),
      mapRpcError(AnyconRpcError.of('busy', 'x')),
      mapRpcError(AnyconRpcError.of('forbidden', 'x', { reason: 'too_large' })),
      mapRpcError(AnyconRpcError.of('unsupported', 'x')),
    ]) {
      assert.ok(err instanceof RpcError && !(err instanceof AttachmentError))
    }
  })
})

for (const impl of cryptoImpls) {
  describe(`图片附件（${impl.label}）`, () => {
    it('uploadImage：put_begin 带文件的事实，512 KiB 一块、最多 2 块在途，put_end 返回 daemon 的记录；downloadAttachment 分页拉回原样', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const img = png(1280, 800, 4 * 512 * KIB + 1000)
        env.daemon.chunkHook = () => sleep(15)
        const progress: Array<[number, number]> = []
        const ref = await node.uploadImage(img, {
          name: 'shot.png',
          mime: 'image/png',
          onProgress: (sent, total) => progress.push([sent, total]),
        })

        assert.deepEqual(paramsOf(env, 'blob.put_begin'), [
          { name: 'shot.png', mime: 'image/png', size: img.length, sha256: sha256(img) },
        ])
        const chunks = paramsOf(env, 'blob.put_chunk')
        assert.deepEqual(chunks.map((c) => c.index).toSorted(), [0, 1, 2, 3, 4])
        for (const c of chunks) {
          const index = c.index as number
          const data = decode(c.data as string)
          assert.equal(data.length, index < 4 ? 512 * KIB : 1000)
          assert.deepEqual(data, img.subarray(index * 512 * KIB, index * 512 * KIB + data.length))
        }
        assert.equal(env.daemon.chunksPeak, 2, 'at most 2 chunks in flight, and it does use both')
        const methods = blobMethods(env)
        assert.equal(methods[0], 'blob.put_begin')
        assert.equal(methods.at(-1), 'blob.put_end')
        assert.deepEqual(ref, {
          blob_id: ref.blob_id,
          name: 'shot.png',
          mime: 'image/png',
          bytes: img.length,
          width: 1280,
          height: 800,
        })
        assert.deepEqual(progress[0], [0, img.length])
        assert.deepEqual(progress.at(-1), [img.length, img.length])
        assert.deepEqual(
          progress.map(([sent]) => sent),
          progress.map(([sent]) => sent).toSorted((a, b) => a - b),
        )

        const down = await node.downloadAttachment(ref)
        assert.deepEqual(down.ref, ref)
        assert.equal(down.bytes.length, img.length)
        assert.ok(Buffer.from(down.bytes).equals(Buffer.from(img)))
        assert.deepEqual(
          paramsOf(env, 'blob.get').map((p) => p.offset),
          [0, 512 * KIB, 1024 * KIB, 1536 * KIB, 2048 * KIB],
        )
        // 也接受 blob_id 字符串；不存在 → not_found（没有 reason，不是 AttachmentError）
        assert.equal((await node.downloadAttachment(ref.blob_id)).bytes.length, img.length)
        await assert.rejects(node.downloadAttachment('f'.repeat(32)), (err: unknown) => {
          assert.ok(err instanceof RpcError && !(err instanceof AttachmentError))
          assert.equal(err.code, 'not_found')
          return true
        })
      })
    })

    it('mime 缺省时按文件头判断；认不出抛 unsupported_image，什么都不发', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const ref = await node.uploadImage(jpeg(3000), { name: 'photo.jpg' })
        assert.equal(ref.mime, 'image/jpeg')
        assert.equal(paramsOf(env, 'blob.put_begin')[0]?.mime, 'image/jpeg')
        // 给的 mime 大小写不敏感，发出去是小写
        const upper = await node.uploadImage(png(2, 2, 100), { name: 'a.png', mime: 'IMAGE/PNG' })
        assert.equal(upper.mime, 'image/png')
        assert.equal(paramsOf(env, 'blob.put_begin')[1]?.mime, 'image/png')

        const before = env.daemon.requests.length
        await assert.rejects(
          node.uploadImage(new TextEncoder().encode('<svg xmlns="x"/>'), { name: 'x.svg' }),
          (err: unknown) => {
            assert.ok(err instanceof AnyconError)
            assert.equal(err.code, 'unsupported_image')
            return true
          },
        )
        assert.equal(env.daemon.requests.length, before)
      })
    })

    it('daemon 的拒绝带 reason：too_large / quota / disk_full 是 AttachmentError、不 discard；uploadImage 给非图片 mime 本地抛 unsupported_image；文件头不符是 mime_mismatch', async () => {
      await withNode(await impl.load(), async (node, env) => {
        env.daemon.maxAttachmentBytes = 1000
        await rejectsAttachment(
          node.uploadImage(png(10, 10, 2000), { name: 'big.png' }),
          'invalid',
          'too_large',
        )
        env.daemon.maxAttachmentBytes = 10 * 1024 * KIB
        env.daemon.blobRefusal = 'quota'
        await rejectsAttachment(
          node.uploadImage(png(10, 10, 500), { name: 'a.png' }),
          'busy',
          'quota',
        )
        env.daemon.blobRefusal = 'disk_full'
        await rejectsAttachment(
          node.uploadImage(png(10, 10, 500), { name: 'a.png' }),
          'busy',
          'disk_full',
        )
        env.daemon.blobRefusal = undefined
        // uploadImage 只传四种图片：别的 mime 在本地挡掉（daemon 已经收任意文件了），什么都不发
        const before = env.daemon.requests.length
        await assert.rejects(
          node.uploadImage(png(10, 10, 500), { name: 'a.bmp', mime: 'image/bmp' }),
          (err: unknown) => {
            assert.ok(err instanceof AnyconError)
            assert.equal(err.code, 'unsupported_image')
            assert.match(err.message, /uploadFile/)
            return true
          },
        )
        assert.equal(env.daemon.requests.length, before)
        // put_begin 被拒：没有上传可丢
        assert.deepEqual(paramsOf(env, 'blob.discard'), [])

        // 声明成 jpeg 的 png：put_end 的文件头校验不过，上传作废
        const err = await rejectsAttachment(
          node.uploadImage(png(10, 10, 500), { name: 'a.jpg', mime: 'image/jpeg' }),
          'invalid',
          'mime_mismatch',
        )
        assert.match(err.message, /image\/jpeg/)
        await until(() => paramsOf(env, 'blob.discard').length === 1, 2000, 'blob.discard')
      })
    })

    it('超时的块自己重试（只重发那一块）；daemon 明确拒绝的块不重试，上传被 discard、不 put_end；块答 finalizing 时直接 put_end 拿结果', async () => {
      await withNode(await impl.load(), async (node, env) => {
        env.daemon.blobChunkSize = 1024
        env.daemon.chunkHook = async (index, attempt) => {
          if (index === 1 && attempt === 1) await sleep(500)
        }
        const img = png(10, 10, 3000)
        const ref = await node.uploadImage(img, { name: 'a.png', requestTimeoutMs: 200 })
        const arrivals = (i: number) => env.daemon.chunkArrivals.get(`${ref.blob_id}:${i}`)
        assert.equal(arrivals(1), 2)
        assert.equal(arrivals(0), 1)
        assert.equal(arrivals(2), 1)
        assert.ok(Buffer.from(env.daemon.blobs.get(ref.blob_id)!.bytes).equals(Buffer.from(img)))

        // 上传已过期（not_found）：不重试、不 put_end，丢掉
        env.daemon.chunkHook = (index) => {
          if (index === 0) throw AnyconRpcError.of('not_found', 'no such upload')
        }
        await assert.rejects(
          node.uploadImage(png(10, 10, 3000, 2), { name: 'b.png' }),
          (err: unknown) => {
            assert.ok(err instanceof RpcError && !(err instanceof AttachmentError))
            assert.equal(err.code, 'not_found')
            return true
          },
        )
        const expired = paramsOf(env, 'blob.put_chunk').at(-1)?.blob_id as string
        assert.equal(env.daemon.chunkArrivals.get(`${expired}:0`), 1, 'a refusal is not retried')
        await until(
          () => paramsOf(env, 'blob.discard').some((p) => p.blob_id === expired),
          2000,
          'blob.discard of the failed upload',
        )
        assert.equal(paramsOf(env, 'blob.put_end').length, 1)

        // finalizing：put_end 已经接手，不再发块、也不重发这一块，直接 put_end 拿结果（这里缺块，结果是 incomplete）
        env.daemon.chunkHook = (index) => {
          if (index === 0) {
            throw AnyconRpcError.of('invalid', 'put_end has started', { reason: 'finalizing' })
          }
        }
        await rejectsAttachment(
          node.uploadImage(png(10, 10, 3000, 3), { name: 'c.png' }),
          'invalid',
          'incomplete',
        )
        const taken = paramsOf(env, 'blob.put_chunk').at(-1)?.blob_id as string
        assert.equal(env.daemon.chunkArrivals.get(`${taken}:0`), 1)
        assert.deepEqual(paramsOf(env, 'blob.put_end').at(-1), { blob_id: taken })
      })
    })

    it('uploadFile：任意文件走同一套分块上传；mime 缺省时按文件头 / 扩展名猜，给了就转小写；非图片没有宽高、不核对文件头；downloadAttachment 原样拉回', async () => {
      await withNode(await impl.load(), async (node, env) => {
        env.daemon.blobChunkSize = 1024
        const pdf = new Uint8Array(2500).fill(0x41)
        pdf.set(ascii('%PDF-1.7\n'))
        const progress: Array<[number, number]> = []
        const ref = await node.uploadFile(pdf, {
          name: 'report.pdf',
          onProgress: (sent, total) => progress.push([sent, total]),
        })
        assert.deepEqual(paramsOf(env, 'blob.put_begin')[0], {
          name: 'report.pdf',
          mime: 'application/pdf',
          size: pdf.length,
          sha256: sha256(pdf),
        })
        assert.deepEqual(
          paramsOf(env, 'blob.put_chunk')
            .map((c) => c.index)
            .toSorted(),
          [0, 1, 2],
        )
        assert.deepEqual(ref, {
          blob_id: ref.blob_id,
          name: 'report.pdf',
          mime: 'application/pdf',
          bytes: pdf.length,
        })
        assert.deepEqual(progress.at(-1), [pdf.length, pdf.length])
        const down = await node.downloadAttachment(ref)
        assert.deepEqual(down.ref, ref)
        assert.ok(Buffer.from(down.bytes).equals(Buffer.from(pdf)))

        // 认不出的文件：application/octet-stream；扩展名像图片也不当图片（不会 mime_mismatch）
        const text = new TextEncoder().encode('not really a picture')
        assert.equal(
          (await node.uploadFile(text, { name: 'fake.png' })).mime,
          'application/octet-stream',
        )
        assert.equal((await node.uploadFile(text, { name: 'notes.md' })).mime, 'text/markdown')
        // 内容是图片：按图片传，daemon 读出宽高
        const img = await node.uploadFile(png(40, 30, 600), { name: 'pic' })
        assert.equal(img.mime, 'image/png')
        assert.equal(img.width, 40)
        assert.equal(img.height, 30)
        // 显式 mime 转小写；显式说成图片但内容不是 → daemon 的 mime_mismatch 照常
        const csv = await node.uploadFile(text, { name: 'x.dat', mime: 'Text/CSV' })
        assert.equal(csv.mime, 'text/csv')
        assert.equal(paramsOf(env, 'blob.put_begin').at(-1)?.mime, 'text/csv')
        await rejectsAttachment(
          node.uploadFile(text, { name: 'x.png', mime: 'image/png' }),
          'invalid',
          'mime_mismatch',
        )
      })
    })

    it('uploadFile：不成形的 mime 本地抛 invalid_mime，什么都不发；从不抛 unsupported_image', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const before = env.daemon.requests.length
        for (const mime of ['pdf', 'text/plain; charset=utf-8', '*/*', '']) {
          await assert.rejects(
            node.uploadFile(Uint8Array.of(1, 2, 3), { name: 'a', mime }),
            (err: unknown) => {
              assert.ok(err instanceof AnyconError, String(err))
              assert.equal(err.code, 'invalid_mime')
              return true
            },
          )
        }
        assert.equal(env.daemon.requests.length, before)
        // 认不出的二进制照样能传
        const ref = await node.uploadFile(Uint8Array.of(0, 1, 2, 3), { name: 'blob' })
        assert.equal(ref.mime, 'application/octet-stream')
      })
    })

    it('uploadFile 和 uploadImage 共用一个队列：同时开始，第二个的 put_begin 在第一个的 put_end 之后；带非图片附件发消息', async () => {
      await withNode(await impl.load(), async (node, env) => {
        env.daemon.blobChunkSize = 1024
        env.daemon.chunkHook = () => sleep(5)
        const [a, b] = await Promise.all([
          node.uploadImage(png(10, 10, 5000), { name: 'one.png' }),
          node.uploadFile(new Uint8Array(5000).fill(7), { name: 'two.zip' }),
        ])
        const methods = blobMethods(env)
        assert.ok(
          methods.lastIndexOf('blob.put_begin') > methods.indexOf('blob.put_end'),
          methods.join(' '),
        )
        assert.equal(b.mime, 'application/zip')

        const session = await node.sessions.create({ agent: 'mock', project: 'app' })
        const events = await collect(session.send('see attached', { attachments: [a, b] }))
        assert.deepEqual(paramsOf(env, 'session.send')[0]?.attachments, [a, b])
        const user = events.find(
          (e): e is Extract<StreamEvent, { type: 'user_message' }> => e.type === 'user_message',
        )
        assert.deepEqual(user?.attachments, [a, b])
      })
    })

    it('同一个节点上的上传排队：同时传两张，第二张的 put_begin 在第一张的 put_end 之后；排队时中止的什么都不发', async () => {
      await withNode(await impl.load(), async (node, env) => {
        env.daemon.blobChunkSize = 1024
        env.daemon.chunkHook = () => sleep(5)
        const ctl = new AbortController()
        const first = node.uploadImage(png(10, 10, 6000, 1), { name: 'one.png' })
        const second = node.uploadImage(png(10, 10, 6000, 2), { name: 'two.png' })
        const third = node.uploadImage(png(10, 10, 6000, 3), {
          name: 'three.png',
          signal: ctl.signal,
        })
        ctl.abort(new Error('changed my mind'))
        await assert.rejects(third, /changed my mind/)
        const [a, b] = await Promise.all([first, second])
        assert.notEqual(a.blob_id, b.blob_id)

        const methods = blobMethods(env)
        assert.equal(
          methods.filter((m) => m === 'blob.put_begin').length,
          2,
          'the aborted one never started',
        )
        const lastBegin = methods.lastIndexOf('blob.put_begin')
        assert.ok(lastBegin > methods.indexOf('blob.put_end'), methods.join(' '))
        assert.ok(env.daemon.chunksPeak <= 2, `peak ${env.daemon.chunksPeak}`)
        assert.deepEqual(
          paramsOf(env, 'blob.put_begin').map((p) => p.name),
          ['one.png', 'two.png'],
        )
      })
    })

    it('中止：不再发块，discard 这个上传，不 put_end；之后的上传照常', async () => {
      await withNode(await impl.load(), async (node, env) => {
        env.daemon.blobChunkSize = 1024
        const ctl = new AbortController()
        env.daemon.chunkHook = async (index) => {
          if (index === 2) ctl.abort()
          await sleep(3)
        }
        await assert.rejects(
          node.uploadImage(png(10, 10, 20_000), { name: 'a.png', signal: ctl.signal }),
        )
        assert.deepEqual(paramsOf(env, 'blob.put_end'), [])
        await until(() => paramsOf(env, 'blob.discard').length === 1, 2000, 'blob.discard')
        assert.ok(paramsOf(env, 'blob.put_chunk').length < 20)
        assert.equal(env.daemon.uploads.size, 0)

        env.daemon.chunkHook = undefined
        const ref = await node.uploadImage(png(10, 10, 3000), { name: 'b.png' })
        assert.equal(ref.name, 'b.png')
      })
    })

    it('send 带附件：线路上带 attachments、user_message 带 daemon 的记录，content 可以为空；引用别的会话的图 → blob_in_use，被丢掉的图 → blob', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const img = png(64, 48, 5000)
        const ref = await node.uploadImage(img, { name: 'shot.png' })
        const session = await node.sessions.create({ agent: 'mock', project: 'app' })
        const events = await collect(session.send('', { attachments: [ref] }))

        const sent = paramsOf(env, 'session.send')
        assert.deepEqual(sent[0]?.attachments, [ref])
        assert.equal(sent[0]?.content, '')
        const user = events.find(
          (e): e is Extract<StreamEvent, { type: 'user_message' }> => e.type === 'user_message',
        )
        assert.deepEqual(user?.attachments, [ref])
        const text = events.map((e) => (e.type === 'text_delta' ? e.text : '')).join('')
        assert.match(text, /Attachments: shot\.png \(image\/png, 5000 bytes\)/)
        // 不带附件时线路上没有这个字段
        await collect(session.send('plain'))
        assert.equal('attachments' in (paramsOf(env, 'session.send')[1] ?? {}), false)

        // 一张图只属于一个会话
        const other = await node.sessions.create({ agent: 'mock', project: 'app' })
        await rejectsAttachment(
          collect(other.send('again', { attachments: [ref] })),
          'invalid',
          'blob_in_use',
        )
        // 已被引用的不能丢；不存在的丢掉也算成功
        await rejectsAttachment(node.discardAttachment(ref), 'invalid', 'in_use')
        await node.discardAttachment('e'.repeat(32))

        // 上传了又丢掉（或 24 小时后被回收）的图：not_found / blob，要重新上传
        const gone = await node.uploadImage(png(8, 8, 800), { name: 'gone.png' })
        await node.discardAttachment(gone)
        const stream = other.send('look', { attachments: [gone] })
        await rejectsAttachment(stream.sent, 'not_found', 'blob')
        await assert.rejects(collect(stream))
      })
    })

    it('sessions.create 带附件不带 prompt：生成 message_id、不发 prompt，第一条 user_message 带附件；同一个 messageId 重试拿到同一个会话', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const ref = await node.uploadImage(png(32, 32, 1200), { name: 'red.png' })
        const session = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          attachments: [ref],
        })
        const params = paramsOf(env, 'session.create')[0]!
        assert.equal('prompt' in params, false)
        assert.deepEqual(params.attachments, [ref])
        assert.match(params.message_id as string, UUID_V4)

        const seen: StreamEvent[] = []
        for await (const ev of session.events({ fromSeq: 0 })) {
          seen.push(ev)
          if (ev.type === 'turn_done') break
        }
        const user = seen.find(
          (e): e is Extract<StreamEvent, { type: 'user_message' }> => e.type === 'user_message',
        )
        assert.equal(user?.content, '')
        assert.deepEqual(user?.attachments, [ref])

        // 超时重试：同一个 messageId 拿到第一次的会话，不会因为附件已绑定而 blob_in_use
        const retried = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          attachments: [ref],
          messageId: params.message_id as string,
        })
        assert.equal(retried.id, session.id)
        // 附件已经绑定到这个会话，别的新会话用不了
        await rejectsAttachment(
          node.sessions.create({ agent: 'mock', project: 'app', prompt: 'hi', attachments: [ref] }),
          'invalid',
          'blob_in_use',
        )
      })
    })
  })
}
