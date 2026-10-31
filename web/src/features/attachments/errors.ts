// 附件相关的提示文案（走 i18n）：添加时的提示、上传失败的原因。
import type { TFunction } from 'i18next'
import { describeError } from '@/lib/errors'
import { formatBytes } from '@/lib/format'
import { RpcCallError } from '@/transport/rpcClient'
import type { AttachNotice } from './composerUploads'

/** 添加附件时要告诉用户的事（toast）。 */
export function describeAttachNotice(n: AttachNotice, t: TFunction): string {
  switch (n.kind) {
    case 'not_image':
      return t('composer.attachNotImage', { name: n.name })
    case 'max':
      return t('composer.attachMax', { count: n.max })
    case 'undecodable':
      return t('composer.attachUndecodable', { name: n.name })
    case 'too_large':
      return t('composer.attachTooLarge', { name: n.name })
    case 'unsupported_format':
      return t('composer.attachUnsupported')
    case 'total':
      return t('composer.attachTotal', { name: n.name, size: formatBytes(n.max) })
    case 'lost':
      return t('message.attachmentLost', { count: n.count })
  }
}

/** 上传失败的原因：先看附件专属的（`blob.put_*` 的 `data.reason`），没有再走通用的错误翻译。 */
export function describeAttachmentError(e: unknown, t: TFunction): string {
  if (e instanceof RpcCallError && e.kind === 'rpc') {
    if (e.code === 'invalid' && e.reason === 'too_large') return t('attachment.errors.tooLarge')
    if (e.code === 'invalid' && (e.reason === 'hash_mismatch' || e.reason === 'mime_mismatch' || e.reason === 'incomplete')) return t('attachment.errors.corrupt')
    if (e.code === 'invalid' && e.reason === 'finalizing') return t('attachment.errors.finalizing')
    if (e.code === 'unsupported') return t('attachment.errors.unsupported')
    // `blob.put_begin` 的 `busy`：没被引用的附件在节点上堆得太多（quota）/ 数据盘快满了（disk_full）/ 同时进行的上传太多（没有 reason）
    if (e.code === 'busy' && e.reason === 'quota') return t('attachment.errors.quota')
    if (e.code === 'busy' && e.reason === 'disk_full') return t('attachment.errors.diskFull')
    if (e.code === 'busy') return t('attachment.errors.busy')
    if (e.code === 'not_found') return t('attachment.errors.expired')
  }
  return describeError(e, t)
}

/**
 * 消息（或新建会话）因为附件被 daemon 拒绝时的说明。`reason` 是 `actions.ts` 的 `AttachmentFailure`
 * （`blob_expired` / `blob_in_use` / `attachments_too_large`）；别的值返回 undefined，由调用方走通用说明。
 */
export function describeAttachmentFailure(reason: string | undefined, t: TFunction): string | undefined {
  switch (reason) {
    case 'blob_expired':
      return t('message.attachmentExpired')
    case 'blob_in_use':
      return t('message.attachmentInUse')
    case 'attachments_too_large':
      return t('message.attachmentsTooLarge')
    default:
      return undefined
  }
}
