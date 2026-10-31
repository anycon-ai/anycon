// 文件附件的图标 + 名字 + 大小（输入区的文件条和消息里的文件条共用）。只显示记录里的元数据，从不内联显示文件内容。
import type { ReactNode } from 'react'
import { FileText } from 'lucide-react'
import { formatBytes } from '@/lib/format'

export function FileLabel({ name, bytes, detail }: { name: string; bytes?: number; detail?: ReactNode }) {
  return (
    <>
      <FileText className="size-5 shrink-0 text-fg-subtle" aria-hidden />
      <span className="flex min-w-0 flex-col text-left">
        <span className="truncate text-xs font-medium text-fg" data-testid="attachment-file-name">
          {name}
        </span>
        <span className="truncate text-[0.6875rem] text-fg-muted" data-testid="attachment-file-size">
          {detail ?? (bytes !== undefined ? formatBytes(bytes) : ' ')}
        </span>
      </span>
    </>
  )
}
