// 看大图的对话框（Radix Dialog：焦点困住、Esc 关闭）：文件名、尺寸 / 大小、下载。消息里的图和输入框里还没发送的图共用。
import { Dialog as RDialog } from 'radix-ui'
import { Download, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { formatBytes } from '@/lib/format'
import { nameForMime } from '../normalize'

export interface ViewerImage {
  url: string
  name: string
  mime: string
  width?: number | null
  height?: number | null
  /** 规整前（还没算出来）可能没有。 */
  bytes?: number
}

export function describeImage(img: Pick<ViewerImage, 'width' | 'height' | 'bytes'>): string {
  const dims = img.width && img.height ? `${img.width}×${img.height}` : null
  return [dims, img.bytes !== undefined ? formatBytes(img.bytes) : null].filter(Boolean).join(' · ')
}

export function ImageViewer({ open, onOpenChange, image }: { open: boolean; onOpenChange: (v: boolean) => void; image: ViewerImage }) {
  const { t } = useTranslation()
  return (
    <RDialog.Root open={open} onOpenChange={onOpenChange}>
      <RDialog.Portal>
        <RDialog.Overlay className="fixed inset-0 z-40 bg-black/60 backdrop-blur-[1px]" />
        <RDialog.Content
          data-testid="attachment-viewer"
          className="fixed left-1/2 top-1/2 z-50 flex max-h-[92dvh] w-[calc(100vw-1.5rem)] max-w-4xl -translate-x-1/2 -translate-y-1/2 flex-col gap-3 rounded-lg border border-border bg-surface p-4 shadow-pop focus:outline-none"
        >
          <div className="flex items-start gap-3 pr-8">
            <div className="min-w-0 flex-1">
              <RDialog.Title className="truncate text-sm font-semibold" title={image.name}>
                {image.name}
              </RDialog.Title>
              <RDialog.Description className="text-xs text-fg-muted" data-testid="attachment-viewer-meta">
                {describeImage(image)}
              </RDialog.Description>
            </div>
            <a href={image.url} download={nameForMime(image.name, image.mime)} data-testid="attachment-download" className="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-md border border-border-strong px-3 text-[0.8125rem] font-medium text-fg hover:bg-bg-hover">
              <Download className="size-4" aria-hidden />
              {t('attachment.download')}
            </a>
          </div>
          <div className="flex min-h-0 flex-1 items-center justify-center overflow-auto rounded-md bg-bg-muted p-1">
            <img src={image.url} alt={image.name} className="max-h-[75dvh] max-w-full object-contain" data-testid="attachment-viewer-image" />
          </div>
          <RDialog.Close aria-label={t('common.close')} className="absolute right-3 top-3 rounded-md p-1.5 text-fg-subtle hover:bg-bg-hover hover:text-fg">
            <X className="size-4" />
          </RDialog.Close>
        </RDialog.Content>
      </RDialog.Portal>
    </RDialog.Root>
  )
}
