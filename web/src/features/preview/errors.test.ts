// 预览错误的说明：daemon 的 data.reason、hub 的错误码、本地条件各有专门的说法，其余走通用说法。
import i18n from 'i18next'
import { describe, expect, it } from 'vitest'
import { ApiError } from '@/api/client'
import { rpcError } from '@/test/fakeRpc'
import { RpcCallError } from '@/transport/rpcClient'
import { describePreviewError } from './errors'
import { PreviewLocalError, type PreviewLocalCode } from './ticket'

const t = i18n.t.bind(i18n)

describe('describePreviewError', () => {
  it('forbidden: port_not_allowed and disabled have their own wording; other forbidden falls back', () => {
    expect(describePreviewError(rpcError('forbidden', undefined, 'port_not_allowed'), t)).toContain('preview.allow_ports')
    expect(describePreviewError(rpcError('forbidden', undefined, 'disabled'), t)).toContain('preview.enabled')
    expect(describePreviewError(rpcError('forbidden'), t)).toBe('The node does not allow this.')
  })

  it('hub codes from the token endpoint', () => {
    expect(describePreviewError(new ApiError(404, 'preview_disabled', 'x'), t)).toBe('This hub has preview turned off.')
    expect(describePreviewError(new ApiError(404, 'not_found', 'x'), t)).toContain('doesn’t know this node')
    expect(describePreviewError(new ApiError(409, 'label_taken', 'x'), t)).toContain('another node')
    expect(describePreviewError(new ApiError(409, 'too_many_labels', 'x'), t)).toContain('maximum')
    expect(describePreviewError(new ApiError(400, 'invalid_label', 'x'), t)).toContain('can’t use')
    expect(describePreviewError(new ApiError(401, 'unauthorized', 'x'), t)).toContain('paired device')
    expect(describePreviewError(new ApiError(401, 'http_401', 'x'), t)).toContain('paired device')
  })

  it.each<PreviewLocalCode>(['no_device_token', 'no_node_key', 'bad_template', 'bad_label'])('local: %s', (code) => {
    const text = describePreviewError(new PreviewLocalError(code), t)
    expect(text).not.toBe('')
    expect(text).not.toContain('preview.errors')
  })

  it('everything else is the usual wording', () => {
    expect(describePreviewError(new RpcCallError('offline', 'x'), t)).toBe('The node is offline. Nothing was sent.')
    expect(describePreviewError(new RpcCallError('timeout', 'x'), t)).toBe('The node did not answer in time.')
    expect(describePreviewError(new ApiError(0, 'network', 'x'), t)).toBe('Could not reach the hub.')
    expect(describePreviewError(new ApiError(429, 'rate_limited', 'x'), t)).toBe('Too many requests. Try again in a moment.')
  })
})
