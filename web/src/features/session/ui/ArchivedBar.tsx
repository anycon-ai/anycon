import type { ReactNode } from 'react'
import { ArchiveRestore, Square } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'

/**
 * 已归档会话的输入区（web.md § 4.6「输入区」）：daemon 对已归档的会话拒绝 `session.send`（`invalid` / `archived`），
 * 所以输入框换成一条「已归档 · 取消归档」。点按钮调 `session.unarchive`，成功后会话不再是归档的，页面换回输入框。
 * daemon 现在在运行 / 等审批时拒绝归档（`busy`），归档的会话不该还有 turn；万一状态仍是运行中（别的设备的竞态、
 * 旧 daemon），条上保留「停止」按钮，不然这个 turn 只能先取消归档才能停。
 * `leading`：放在说明前面（会话页的分支 chip：worktree 保留 / 已删除）。
 */
export function ArchivedBar({ onUnarchive, busy, disabled, running = false, onStop, leading }: { onUnarchive: () => void; busy: boolean; disabled: boolean; running?: boolean; onStop?: () => void; leading?: ReactNode }) {
  const { t } = useTranslation()
  return (
    <div className="shrink-0 border-t border-border bg-bg px-3 pb-[calc(1.5rem+env(safe-area-inset-bottom))] pt-2 md:px-4" data-testid="session-archived-bar">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        {leading}
        <p className="min-w-0 flex-1 text-sm text-fg-muted" role="status" data-testid="session-archived-hint">
          {t('session.archivedHint')}
        </p>
        {running && onStop ? (
          <Button variant="danger" size="md" icon={<Square className="size-4 fill-current" aria-hidden />} disabled={disabled} onClick={onStop} data-testid="session-archived-stop">
            {t('composer.stop')}
          </Button>
        ) : null}
        <Button variant="outline" size="md" icon={<ArchiveRestore className="size-4" aria-hidden />} loading={busy} disabled={disabled} onClick={onUnarchive} data-testid="session-unarchive">
          {t('info.unarchive')}
        </Button>
      </div>
    </div>
  )
}
