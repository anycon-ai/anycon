import type { ReactNode } from 'react'
import { Link, useCanGoBack, useNavigate, useRouter } from '@tanstack/react-router'
import { ArrowLeft, ChevronRight, Menu, Pencil } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useCurrentNode } from '@/lib/nav'
import { useNodeFilter } from '@/lib/nodeFilter'
import { useUi } from '@/stores/ui'
import { SpaceSwitcher } from './SpaceSwitcher'
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
 * 页面顶栏：手机上的菜单按钮（打开左侧抽屉）和返回按钮、面包屑、页面的状态（`meta`，比如会话页的「聊天 / 改动」和状态 chip）、
 * 节点状态 chip、页面自己的操作按钮。
 * `root`：左栏里直接能到的页面（待处理、清单、会话……），手机上只有菜单按钮；其余是从别的页面点进来的下一级页，菜单旁边多一个返回。
 * 页面传了 `onBack` 时（文件预览回到树）不管 `root` 都显示返回。
 * `tabs`：同一区域里并列的几页（Twin 和规则）放一排标签，代替面包屑。
 */
export function PageHeader({ crumbs = [], tabs, meta, actions, showNodeStatus = true, onBack, root, spaceSwitcher = false }: { crumbs?: Crumb[]; tabs?: ReactNode; meta?: ReactNode; actions?: ReactNode; showNodeStatus?: boolean; onBack?: () => void; root?: boolean; /** 手机上顶栏带当前空间的切换按钮（只在有分组时）：跨节点汇总的页面（待处理、清单、Twin）用，免得为了换空间去开抽屉。 */ spaceSwitcher?: boolean }) {
  const { t } = useTranslation()
  const { entry, nodeId } = useCurrentNode()
  const hasGroups = useNodeFilter().spaces.hasGroups
  const setDrawer = useUi((s) => s.setLeftDrawer)
  return (
    <header className="pt-safe flex min-h-12 shrink-0 items-center gap-2 border-b border-border bg-bg px-3">
      <button type="button" aria-label={t('sidebar.menu')} className="-ml-2 grid size-11 shrink-0 place-items-center rounded-md text-fg-muted hover:bg-bg-hover md:hidden" onClick={() => setDrawer(true)} data-testid="menu-button">
        <Menu className="size-5" aria-hidden />
      </button>
      {root && !onBack ? null : <BackButton onBack={onBack} />}
      {spaceSwitcher && hasGroups ? <SpaceSwitcher nodeId={nodeId} variant="topbar" /> : null}
      {tabs ?? (
      <nav aria-label={t('common.breadcrumb')} className="flex min-w-0 flex-1 items-center gap-1 text-sm">
        {crumbs.map((c, i) => (
          // 前面几级不参与收缩，每级最多占面包屑区的 30%；放不下时截断的是最后一级。都按比例收缩的话，
          // 一个长标题会把很短的节点名、项目名也挤成一两个字
          <span key={`${i}-${c.label}`} className={i === crumbs.length - 1 ? 'flex min-w-0 items-center gap-1' : 'hidden max-w-[30%] shrink-0 items-center gap-1 sm:flex'}>
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
      )}
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
