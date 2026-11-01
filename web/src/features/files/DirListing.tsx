// 选中的是目录（面包屑里点了一级目录、从项目页或会话信息面板进来）时，预览区显示这个目录里有什么。
import { useEffect, useMemo } from 'react'
import { File, FileImage, Folder } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { formatBytes } from '@/lib/format'
import type { DirCache } from './hooks'
import { ErrorBlock, Loading, PathLink } from './parts'
import { fileKind, isHidden, joinPath } from './paths'

export function DirListing({ node, path, cache, showHidden, onRetry }: { node: string; path: string; cache: DirCache; showHidden: boolean; onRetry: () => void }) {
  const { t, i18n } = useTranslation()
  const { dirs, load } = cache
  useEffect(() => {
    load(path)
  }, [path, load])
  const st = dirs[path]
  const entries = useMemo(() => (st?.entries ?? []).filter((e) => showHidden || !isHidden(e.name)), [st?.entries, showHidden])
  if (!st || (st.status === 'loading' && !st.entries)) return <Loading />
  if (st.status === 'error') return <ErrorBlock error={st.error} onRetry={onRetry} />
  if (entries.length === 0)
    return (
      <div className="flex flex-1 items-center justify-center p-8 text-sm text-fg-muted" data-testid="files-empty-dir">
        {t('files.emptyFolder')}
      </div>
    )
  const fmt = new Intl.DateTimeFormat(i18n.language, { dateStyle: 'medium', timeStyle: 'short' })
  return (
    <div className="scroll-thin min-h-0 flex-1 overflow-auto" data-testid="files-dir">
      <table className="w-full text-sm">
        <thead className="sticky top-0 bg-bg text-left text-xs text-fg-subtle">
          <tr className="border-b border-border">
            <th className="px-3 py-2 font-medium">{t('files.name')}</th>
            <th className="w-24 px-3 py-2 text-right font-medium">{t('files.size')}</th>
            <th className="hidden w-44 px-3 py-2 font-medium sm:table-cell">{t('files.modified')}</th>
          </tr>
        </thead>
        <tbody>
          {entries.map((e) => {
            const p = joinPath(path, e.name)
            const Icon = e.is_dir ? Folder : fileKind(e.name).type === 'image' ? FileImage : File
            return (
              <tr key={e.name} className="border-b border-border/60 hover:bg-bg-hover" data-testid="files-dir-row">
                <td className="max-w-0 px-3 py-1.5">
                  <PathLink node={node} path={p} className="flex min-w-0 items-center gap-2 hover:underline" testId="files-dir-link">
                    <Icon className="size-4 shrink-0 text-fg-subtle" aria-hidden />
                    <span className="truncate">{e.name}</span>
                  </PathLink>
                </td>
                <td className="px-3 py-1.5 text-right text-xs text-fg-subtle">{e.is_dir ? '' : formatBytes(e.bytes)}</td>
                <td className="hidden px-3 py-1.5 text-xs text-fg-subtle sm:table-cell">{e.modified_at_ms ? fmt.format(e.modified_at_ms) : ''}</td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}
