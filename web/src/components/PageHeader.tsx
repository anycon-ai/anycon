import type { ReactNode } from 'react'
import { Link, useCanGoBack, useNavigate, useRouter } from '@tanstack/react-router'
import { ArrowLeft, ChevronRight, Pencil } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useCurrentNode } from '@/lib/nav'
import { Chip } from '@/ui/Misc'
import { connectionLabel, connectionTone } from './useConnectionLabel'

export interface Crumb {
  label: string
  to?: { to: string; params?: Record<string, string> }
  /** 最后一级可点（会话页：改标题）：显示成按钮，悬停出现铅笔。 */
  onClick?: () => void
  /** `onClick` 按钮的可访问名称和 data-testid。 */
  actionLabel?: string
  testId?: string
}

/**
 * 页面顶栏：手机上的返回按钮、面包屑、页面的状态（`meta`，比如会话页的「聊天 / 改动」和状态 chip）、
 * 节点状态 chip、页面自己的操作按钮。
 */
export function PageHeader({ crumbs, meta, actions, showNodeStatus = true, onBack, root }: { crumbs: Crumb[]; meta?: ReactNode; actions?: ReactNode; showNodeStatus?: boolean; onBack?: () => void; root?: boolean }) {
  const { t } = useTranslation()
  const { entry } = useCurrentNode()
  return (
    <header className="pt-safe flex min-h-12 shrink-0 items-center gap-2 border-b border-border bg-bg px-3">
      {/* 手机没有左侧抽屉：底部标签栏页（root）没有返回，其余页面都是二级页，带返回（默认走浏览器历史） */}
      {root ? null : <BackButton onBack={onBack} />}
      <nav aria-label={t('common.breadcrumb')} className="flex min-w-0 flex-1 items-center gap-1 text-sm">
        {crumbs.map((c, i) => (
          <span key={`${i}-${c.label}`} className={i === crumbs.length - 1 ? 'flex min-w-0 items-center gap-1' : 'hidden min-w-0 items-center gap-1 sm:flex'}>
            {i > 0 ? <ChevronRight className="size-3.5 shrink-0 text-fg-subtle" aria-hidden /> : null}
            {c.onClick ? (
              <button type="button" onClick={c.onClick} aria-label={c.actionLabel} data-testid={c.testId} className="group flex min-w-0 items-center gap-1.5 rounded-md px-1 py-0.5 text-left hover:bg-bg-hover">
                <h1 className="truncate font-medium">{c.label}</h1>
                <Pencil className="size-3 shrink-0 text-fg-subtle opacity-0 group-hover:opacity-100" aria-hidden />
              </button>
            ) : c.to ? (
              <Link to={c.to.to} params={c.to.params as never} className="truncate text-fg-muted hover:text-fg">
                {c.label}
              </Link>
            ) : (
              <span className={i === crumbs.length - 1 ? 'truncate font-medium' : 'truncate text-fg-muted'}>{c.label}</span>
            )}
          </span>
        ))}
      </nav>
      {meta}
      {showNodeStatus && entry ? (
        <Chip tone={connectionTone(entry.conn)} testId="node-status" dataState={entry.conn} title={connectionLabel(entry.conn, t)} dotOnMobile className="shrink-0">
          {connectionLabel(entry.conn, t)}
        </Chip>
      ) : null}
      {actions}
    </header>
  )
}

/** 手机上的返回按钮。默认走浏览器历史（手势返回也是同一条路）；没有上一页（从通知或书签直接进来）时回「待处理」。 */
function BackButton({ onBack }: { onBack?: () => void }) {
  const { t } = useTranslation()
  const router = useRouter()
  const canGoBack = useCanGoBack()
  const navigate = useNavigate()
  const back = (): void => {
    if (onBack) onBack()
    else if (canGoBack) router.history.back()
    else void navigate({ to: '/inbox', replace: true })
  }
  return (
    <button type="button" onClick={back} className="grid size-11 shrink-0 place-items-center rounded-md text-fg-muted hover:bg-bg-hover md:hidden" aria-label={t('common.back')} data-testid="header-back">
      <ArrowLeft className="size-5" />
    </button>
  )
}
