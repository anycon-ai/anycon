// 输入框里的附件条（spec/web.md § 4.6「上传」）：图片是 64 px 缩略图（点开看大图），其它文件是文件条（图标、名字、大小，不能点开）。
// 状态 uploading / ready / error，右上角移除，失败的可以重试。
import { useState } from 'react'
import { ImageIcon, Loader2, RotateCw, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { AttachmentItem } from '@/stores/drafts'
import { cn } from '@/ui/cn'
import { describeAttachmentError } from '../errors'
import { useBlobUrl } from '../hooks'
import { FileLabel } from './FileLabel'
import { ImageViewer } from './ImageViewer'

interface ChipProps {
  nodeId: string
  item: AttachmentItem
  /** 当前选的 agent 不收它（换了 agent）：调暗，发送时不带。 */
  unsupported?: boolean
  onRemove: () => void
  onRetry: () => void
}

/** 上传中的进度遮罩、失败的重试、右上角的移除：图片和文件共用。 */
function ChipControls({ item, onRemove, onRetry }: Pick<ChipProps, 'item' | 'onRemove' | 'onRetry'>) {
  const { t } = useTranslation()
  const percent = Math.round(item.progress * 100)
  return (
    <>
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
    </>
  )
}

const chipClass = (item: AttachmentItem, unsupported?: boolean): string => cn('relative shrink-0 overflow-hidden rounded-lg border bg-bg-muted', item.state === 'error' ? 'border-danger/60' : 'border-border', unsupported && 'opacity-50')

function ImageThumb({ nodeId, item, unsupported, onRemove, onRetry }: ChipProps) {
  const { t } = useTranslation()
  // 刚选的图有本地预览；从草稿恢复的没有，从附件缓存取
  const cached = useBlobUrl(nodeId, item.previewUrl ? undefined : item.ref?.blob_id, true)
  const src = item.previewUrl ?? cached.url
  const [open, setOpen] = useState(false)
  return (
    <div data-testid="composer-attachment" data-kind="image" data-state={item.state} data-name={item.name} data-unsupported={unsupported || undefined} title={item.name} className={cn(chipClass(item, unsupported), 'size-16')}>
      {src ? (
        <button type="button" onClick={() => setOpen(true)} aria-label={t('attachment.open', { name: item.name })} data-testid="composer-attachment-open" className="block size-full cursor-zoom-in focus-visible:outline focus-visible:outline-2 focus-visible:outline-accent">
          <img src={src} alt={item.name} className="size-full object-cover" draggable={false} />
        </button>
      ) : (
        <ImageIcon className="absolute inset-0 m-auto size-6 text-fg-subtle" aria-hidden />
      )}
      <ChipControls item={item} onRemove={onRemove} onRetry={onRetry} />
      {src ? <ImageViewer open={open} onOpenChange={setOpen} image={{ url: src, name: item.name, mime: item.mime, width: item.width, height: item.height, bytes: item.bytes }} /> : null}
    </div>
  )
}

/** 文件条：只显示名字和大小，不预览内容，点了也不打开什么。 */
function FileChip({ item, unsupported, onRemove, onRetry }: ChipProps) {
  return (
    <div data-testid="composer-attachment" data-kind="file" data-state={item.state} data-name={item.name} data-unsupported={unsupported || undefined} title={item.name} className={cn(chipClass(item, unsupported), 'flex h-16 w-52 max-w-full items-center gap-2 pl-2.5 pr-7')}>
      <FileLabel name={item.name} bytes={item.ref?.bytes ?? item.bytes} />
      <ChipControls item={item} onRemove={onRemove} onRetry={onRetry} />
    </div>
  )
}

export function ComposerAttachments({ nodeId, items, unsupported, onRemove, onRetry }: { nodeId: string; items: readonly AttachmentItem[]; unsupported?: (item: AttachmentItem) => boolean; onRemove: (id: string) => void; onRetry: (id: string) => void }) {
  const { t } = useTranslation()
  const failed = items.find((i) => i.state === 'error')
  return (
    <div className="px-3 pt-3" data-testid="composer-attachments">
      <ul className="flex flex-wrap gap-2" aria-label={t('composer.attachments')}>
        {items.map((item) => {
          const props: ChipProps = { nodeId, item, unsupported: unsupported?.(item), onRemove: () => onRemove(item.id), onRetry: () => onRetry(item.id) }
          return <li key={item.id}>{item.kind === 'file' ? <FileChip {...props} /> : <ImageThumb {...props} />}</li>
        })}
      </ul>
      {failed ? (
        <p className="mt-1 text-xs text-danger" role="alert" data-testid="composer-attachment-error">
          {t('composer.attachmentFailed', { name: failed.name, reason: describeAttachmentError(failed.error, t) })}
        </p>
      ) : null}
    </div>
  )
}
