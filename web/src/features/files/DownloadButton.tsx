// 文件预览头部的「下载」按钮（docs/spec/web.md § 4.7）：`fs.read` 分块把文件读完，再存盘。
// 下载中点一下就取消；超过 [`DOWNLOAD_MAX_BYTES`] 的文件不读（已知大小的直接禁用，不知道的读到第一块就停）。
import { useEffect, useRef, useState } from 'react'
import { Download, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { saveBytes } from '@/features/attachments/download'
import { sanitizeName } from '@/features/attachments/normalize'
import { formatBytes } from '@/lib/format'
import { toast } from '@/stores/ui'
import { DOWNLOAD_MAX_BYTES, readFileBytes } from './api'
import { describeFsError } from './errors'
import { baseName } from './paths'

const BUTTON = 'inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-xs text-fg-subtle hover:bg-bg-hover hover:text-fg disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent disabled:hover:text-fg-subtle'

/** `size`：目录列表里已知的大小；没有就等读第一块才知道。 */
export function DownloadButton({ nodeId, path, size }: { nodeId: string; path: string; size?: number }) {
  const { t } = useTranslation()
  const [progress, setProgress] = useState<{ loaded: number; total: number } | null>(null)
  const [busy, setBusy] = useState(false)
  const controller = useRef<AbortController | null>(null)
  // 换文件（预览按文件重新挂载）、离开页面就中止，后面的块不再请求
  useEffect(() => () => controller.current?.abort(), [])

  const tooLarge = size !== undefined && size > DOWNLOAD_MAX_BYTES
  const tooLargeText = (total: number): string => t('files.downloadTooLarge', { size: formatBytes(total), limit: formatBytes(DOWNLOAD_MAX_BYTES) })

  const start = (): void => {
    const ctl = new AbortController()
    controller.current = ctl
    setBusy(true)
    setProgress(null)
    readFileBytes(nodeId, path, { signal: ctl.signal, maxBytes: DOWNLOAD_MAX_BYTES, onProgress: (loaded, total) => setProgress({ loaded, total }) }).then(
      (file) => {
        if (ctl.signal.aborted) return
        if (file.truncated) toast.error(tooLargeText(file.total))
        else saveBytes(file.bytes, sanitizeName(baseName(path), 'file'))
      },
      (e: unknown) => {
        if (ctl.signal.aborted) return
        console.warn('anycon: could not download the file', e)
        toast.error(`${t('files.downloadFailed')} ${describeFsError(e, t)}`)
      },
    ).finally(() => {
      if (controller.current !== ctl) return
      controller.current = null
      setBusy(false)
      setProgress(null)
    })
  }

  if (busy)
    return (
      <button type="button" className={BUTTON} onClick={() => controller.current?.abort()} data-testid="file-download-cancel" aria-label={t('files.downloadCancel')} title={t('files.downloadCancel')}>
        <X className="size-3.5" aria-hidden />
        <span aria-live="polite" data-testid="file-download-progress">
          {progress && progress.total > 0 ? `${formatBytes(progress.loaded)} / ${formatBytes(progress.total)}` : t('files.downloading')}
        </span>
      </button>
    )
  return (
    <button
      type="button"
      className={BUTTON}
      onClick={start}
      disabled={tooLarge}
      aria-label={t('files.download')}
      title={tooLarge ? tooLargeText(size) : t('files.download')}
      data-testid="file-download"
    >
      <Download className="size-3.5" aria-hidden />
    </button>
  )
}
