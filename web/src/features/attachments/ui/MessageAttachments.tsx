// 消息里的图片（spec/web.md § 4.6「显示」）：按 daemon 记录的宽高占好位置，进入视口才去拉（先查 IndexedDB 缓存，没有再 `blob.get`），
// 点开是对话框大图（文件名、尺寸、下载）。拉不到显示「图片不可用」，点重试。
import { useState } from 'react'
import { ImageIcon, ImageOff, Loader2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { AttachmentRef } from '@/protocol'
import { cn } from '@/ui/cn'
import { useBlobUrl, useInView } from '../hooks'
import { ImageViewer } from './ImageViewer'

/** 缩略图的最大边（px）。 */
const MAX_THUMB = 200
const MIN_THUMB = 64
const FALLBACK_THUMB = 160

/** 缩略图占位的大小：按宽高比缩进 200 px 的方框，不放大；没有宽高（daemon 读不出）就用方块。 */
export function thumbBox(width?: number | null, height?: number | null): { w: number; h: number } {
  if (!width || !height || width <= 0 || height <= 0) return { w: FALLBACK_THUMB, h: FALLBACK_THUMB }
  const scale = Math.min(1, MAX_THUMB / width, MAX_THUMB / height)
  return { w: Math.max(MIN_THUMB, Math.round(width * scale)), h: Math.max(MIN_THUMB, Math.round(height * scale)) }
}

export function MessageAttachment({ nodeId, att }: { nodeId?: string; att: AttachmentRef }) {
  const { t } = useTranslation()
  const [boxRef, seen] = useInView<HTMLDivElement>()
  const img = useBlobUrl(nodeId, att.blob_id, seen)
  const [open, setOpen] = useState(false)
  const box = thumbBox(att.width, att.height)
  return (
    <div
      ref={boxRef}
      data-testid="message-attachment"
      data-state={img.status}
      data-name={att.name}
      className="relative max-w-full overflow-hidden rounded-lg border border-border bg-bg-muted"
      style={{ width: box.w, aspectRatio: `${box.w} / ${box.h}` }}
    >
      {img.status === 'ready' && img.url ? (
        <button type="button" onClick={() => setOpen(true)} aria-label={t('attachment.open', { name: att.name })} className="block size-full focus-visible:outline focus-visible:outline-2 focus-visible:outline-accent">
          <img src={img.url} alt={att.name} loading="lazy" width={att.width ?? undefined} height={att.height ?? undefined} className="size-full object-contain" draggable={false} />
        </button>
      ) : img.status === 'error' ? (
        <div className="flex size-full flex-col items-center justify-center gap-1 p-2 text-center text-xs text-fg-muted" data-testid="message-attachment-error">
          <ImageOff className="size-5 text-fg-subtle" aria-hidden />
          <span>{t('attachment.unavailable')}</span>
          <button type="button" onClick={img.retry} className="rounded px-1.5 py-0.5 font-medium text-accent-text hover:bg-bg-hover" data-testid="message-attachment-retry">
            {t('attachment.retry')}
          </button>
        </div>
      ) : (
        <div className="flex size-full items-center justify-center" role="img" aria-label={t('attachment.loading', { name: att.name })}>
          {img.status === 'loading' ? <Loader2 className="size-4 animate-spin text-fg-subtle" aria-hidden /> : <ImageIcon className="size-5 text-fg-subtle" aria-hidden />}
        </div>
      )}
      {img.status === 'ready' && img.url ? <ImageViewer open={open} onOpenChange={setOpen} image={{ url: img.url, name: att.name, mime: att.mime, width: att.width, height: att.height, bytes: att.bytes }} /> : null}
    </div>
  )
}

/** 消息气泡上方的缩略图网格。 */
export function MessageAttachments({ nodeId, attachments, className }: { nodeId?: string; attachments: readonly AttachmentRef[]; className?: string }) {
  if (attachments.length === 0) return null
  return (
    <div className={cn('flex max-w-[min(85%,56rem)] flex-wrap items-start justify-end gap-2', className)} data-testid="message-attachments">
      {attachments.map((a, i) => (
        <MessageAttachment key={`${a.blob_id}-${i}`} nodeId={nodeId} att={a} />
      ))}
    </div>
  )
}
