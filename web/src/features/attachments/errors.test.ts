import { describe, expect, it } from 'vitest'
import i18n from '@/i18n'
import { rpcError } from '@/test/fakeRpc'
import { RpcCallError } from '@/transport/rpcClient'
import { describeAttachmentError, describeAttachmentFailure, describeAttachNotice } from './errors'

const t = i18n.t.bind(i18n)

describe('describeAttachNotice', () => {
  it('has a sentence for every reason an image can be turned away', () => {
    expect(describeAttachNotice({ kind: 'not_image', name: 'a.txt' }, t)).toBe('Only images can be attached. Skipped a.txt.')
    expect(describeAttachNotice({ kind: 'max', max: 10 }, t)).toBe('You can attach up to 10 images to one message.')
    expect(describeAttachNotice({ kind: 'undecodable', name: 'b.png' }, t)).toBe("Couldn't read b.png as an image.")
    expect(describeAttachNotice({ kind: 'too_large', name: 'c.png' }, t)).toBe('c.png is still too large after shrinking it.')
    expect(describeAttachNotice({ kind: 'unsupported_format', name: 'd.png' }, t)).toContain("doesn't accept")
    expect(describeAttachNotice({ kind: 'total', name: 'e.png', max: 15 * 1024 * 1024 }, t)).toBe('Skipped e.png: the images on one message can add up to 15 MiB at most.')
    expect(describeAttachNotice({ kind: 'lost', count: 1 }, t)).toBe('1 image could not be restored. Add it again.')
    expect(describeAttachNotice({ kind: 'lost', count: 3 }, t)).toBe('3 images could not be restored. Add them again.')
  })
})

describe('describeAttachmentFailure', () => {
  it('has a sentence for each way the node can turn a message down because of its images', () => {
    expect(describeAttachmentFailure('blob_expired', t)).toContain('no longer on the node')
    expect(describeAttachmentFailure('blob_in_use', t)).toContain('already belong to another session')
    expect(describeAttachmentFailure('attachments_too_large', t)).toContain('add up to more than the node accepts')
    expect(describeAttachmentFailure('internal', t)).toBeUndefined()
    expect(describeAttachmentFailure(undefined, t)).toBeUndefined()
  })
})

describe('describeAttachmentError', () => {
  it('translates the reasons blob.put_* gives, before falling back to the generic error text', () => {
    expect(describeAttachmentError(rpcError('invalid', undefined, 'too_large'), t)).toContain('larger than this node accepts')
    expect(describeAttachmentError(rpcError('invalid', undefined, 'hash_mismatch'), t)).toContain('damaged')
    expect(describeAttachmentError(rpcError('invalid', undefined, 'mime_mismatch'), t)).toContain('damaged')
    expect(describeAttachmentError(rpcError('unsupported'), t)).toContain('format')
    expect(describeAttachmentError(rpcError('busy'), t)).toContain('busy')
    expect(describeAttachmentError(rpcError('not_found'), t)).toContain('expired')
    // blob.put_begin 的 busy 有三种：同时进行的上传太多（没有 reason）、没发送的图堆太多、数据盘快满了
    expect(describeAttachmentError(rpcError('busy', undefined, 'quota'), t)).toContain("haven't been sent yet")
    expect(describeAttachmentError(rpcError('busy', undefined, 'disk_full'), t)).toContain('disk is almost full')
    expect(describeAttachmentError(rpcError('invalid', undefined, 'finalizing'), t)).toContain('finishing this upload')
    expect(describeAttachmentError(new RpcCallError('timeout', 'x'), t)).toBe('The node did not answer in time.')
    expect(describeAttachmentError(new RpcCallError('offline', 'x'), t)).toContain('offline')
    expect(describeAttachmentError(rpcError('invalid'), t)).toBe('The request was not valid.')
  })

  it('speaks Chinese when the UI does', async () => {
    await i18n.changeLanguage('zh')
    try {
      expect(describeAttachmentError(rpcError('invalid', undefined, 'too_large'), t)).toBe('图片超过了这个节点接受的大小。')
      expect(describeAttachNotice({ kind: 'max', max: 10 }, t)).toBe('一条消息最多带 10 张图片。')
      expect(describeAttachNotice({ kind: 'total', name: 'e.png', max: 15 * 1024 * 1024 }, t)).toBe('已忽略 e.png：一条消息的图片合计最多 15 MiB。')
      expect(describeAttachNotice({ kind: 'lost', count: 2 }, t)).toBe('有 2 张图片没能恢复，请重新添加。')
      expect(describeAttachmentError(rpcError('busy', undefined, 'quota'), t)).toBe('节点上堆着太多还没发送的图片。先发送或移除一些，或者稍后再试。')
      expect(describeAttachmentError(rpcError('busy', undefined, 'disk_full'), t)).toBe('节点的磁盘快满了，暂时收不了更多图片。')
      expect(describeAttachmentError(rpcError('invalid', undefined, 'finalizing'), t)).toBe('节点正在完成这次上传，请重试。')
      expect(describeAttachmentFailure('blob_in_use', t)).toContain('已经属于另一个会话')
      expect(describeAttachmentFailure('attachments_too_large', t)).toContain('加起来超过了节点接受的大小')
    } finally {
      await i18n.changeLanguage('en')
    }
  })
})
