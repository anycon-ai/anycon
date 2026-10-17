// 输入框里的缩略图条（spec/web.md § 4.6「上传」）：每张 64 px，状态 uploading / ready / error，点开看大图，右上角移除，失败的可以重试。
import { useState } from 'react'
import { ImageIcon, Loader2, RotateCw, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { AttachmentItem } from '@/stores/drafts'
import { cn } from '@/ui/cn'
import { describeAttachmentError } from '../errors'
import { useBlobUrl } from '../hooks'
import { ImageViewer } from './ImageViewer'

function Thumb({ nodeId, item, onRemove, onRetry }: { nodeId: string; item: AttachmentItem; onRemove: () => void; onRetry: () => void }) {
  const { t } = useTranslation()
  // 刚选的图有本地预览；从草稿恢复的没有，从图片缓存取
  const cached = useBlobUrl(nodeId, item.previewUrl ? undefined : item.ref?.blob_id, true)
  const src = item.previewUrl ?? cached.url
  const percent = Math.round(item.progress * 100)
  const [open, setOpen] = useState(false)
  return (
    <div
      data-testid="composer-attachment"
      data-state={item.state}
      data-name={item.name}
      title={item.name}
      className={cn('relative size-16 shrink-0 overflow-hidden rounded-lg border bg-bg-muted', item.state === 'error' ? 'border-danger/60' : 'border-border')}
    >
      {src ? (
        <button type="button" onClick={() => setOpen(true)} aria-label={t('attachment.open', { name: item.name })} data-testid="composer-attachment-open" className="block size-full cursor-zoom-in focus-visible:outline focus-visible:outline-2 focus-visible:outline-accent">
          <img src={src} alt={item.name} className="size-full object-cover" draggable={false} />
        </button>
      ) : (
        <ImageIcon className="absolute inset-0 m-auto size-6 text-fg-subtle" aria-hidden />
      )}
      {item.state === 'uploading' ? (
        // 进度遮罩不挡点击：上传中也能点开看大图
        <div className="pointer-events-none absolute inset-0 flex flex-col items-center justify-center gap-0.5 bg-bg/60 text-[0.6875rem] font-medium text-fg" role="progressbar" aria-label={t('composer.attachmentUploading', { name: item.name, percent })} aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent} data-testid="composer-attachment-progress">
          <Loader2 className="size-4 animate-spin" aria-hidden />
          <span>{percent}%</span>
        </div>
      ) : null}
      {item.state === 'error' ? (
        <button type="button" onClick={onRetry} aria-label={t('composer.attachmentRetry', { name: item.name })} title={describeAttachmentError(item.error, t)} data-testid="composer-attachment-retry" className="absolute inset-0 flex items-center justify-center bg-danger-subtle/80 text-danger hover:bg-danger-subtle">
          <RotateCw className="size-5" aria-hidden />
        </button>
      ) : null}
      <button type="button" onClick={onRemove} aria-label={t('composer.attachmentRemove', { name: item.name })} data-testid="composer-attachment-remove" className="absolute right-0.5 top-0.5 flex size-5 items-center justify-center rounded-full bg-bg/85 text-fg shadow-sm hover:bg-bg before:absolute before:-inset-2 before:content-['']">
        <X className="size-3" aria-hidden />
      </button>
      {src ? <ImageViewer open={open} onOpenChange={setOpen} image={{ url: src, name: item.name, mime: item.mime, width: item.width, height: item.height, bytes: item.bytes }} /> : null}
    </div>
  )
}

export function ComposerAttachments({ nodeId, items, onRemove, onRetry }: { nodeId: string; items: readonly AttachmentItem[]; onRemove: (id: string) => void; onRetry: (id: string) => void }) {
  const { t } = useTranslation()
  const failed = items.find((i) => i.state === 'error')
  return (
    <div className="px-3 pt-3" data-testid="composer-attachments">
      <ul className="flex flex-wrap gap-2" aria-label={t('composer.attachments')}>
        {items.map((item) => (
          <li key={item.id}>
            <Thumb nodeId={nodeId} item={item} onRemove={() => onRemove(item.id)} onRetry={() => onRetry(item.id)} />
          </li>
        ))}
      </ul>
      {failed ? (
        <p className="mt-1 text-xs text-danger" role="alert" data-testid="composer-attachment-error">
          {t('composer.attachmentFailed', { name: failed.name, reason: describeAttachmentError(failed.error, t) })}
        </p>
      ) : null}
    </div>
  )
}
