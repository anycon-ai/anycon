import { useMemo } from 'react'
import { Diff, Hunk, parseDiff } from 'react-diff-view'
import { useTranslation } from 'react-i18next'
import { normalizeDiff } from '../text'

/** unified diff 渲染（react-diff-view）。解析失败时退回纯文本。 */
export function DiffView({ diff }: { diff: string }) {
  const { t } = useTranslation()
  const files = useMemo(() => {
    try {
      return parseDiff(normalizeDiff(diff), { nearbySequences: 'zip' })
    } catch {
      return null
    }
  }, [diff])
  if (!files || files.length === 0)
    return (
      <pre className="scroll-thin overflow-x-auto p-3 font-mono text-xs" aria-label={t('tool.diff')}>
        {diff}
      </pre>
    )
  return (
    <div className="scroll-thin overflow-x-auto" aria-label={t('tool.diff')}>
      {files.map((f, i) => {
        // 一次改了几个文件（Codex fileChange）时，每个文件前面标出文件名
        const path = f.type === 'delete' ? f.oldPath : f.newPath
        return (
          <div key={`${f.oldRevision}-${f.newRevision}-${i}`} data-testid="diff-file" data-path={path}>
            {files.length > 1 && path ? <p className="border-b border-border bg-bg-muted px-3 py-1 font-mono text-xs text-fg-muted">{path}</p> : null}
            <Diff viewType="unified" diffType={f.type} hunks={f.hunks}>
              {(hunks) => hunks.map((h) => <Hunk key={h.content} hunk={h} />)}
            </Diff>
          </div>
        )
      })}
    </div>
  )
}
