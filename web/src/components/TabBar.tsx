import type { ComponentType } from 'react'
import { Link } from '@tanstack/react-router'
import { Inbox, LayoutList, ListChecks, Menu, Plus } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useCapture } from '@/features/todos/capture'
import { useTodoAvailable } from '@/features/todos/store'
import { cn } from '@/ui/cn'

export type TabId = 'inbox' | 'todos' | 'sessions' | 'more'

type Icon = ComponentType<{ className?: string; 'aria-hidden'?: boolean }>

interface TabDef {
  id: TabId
  to: '/inbox' | '/todos' | '/sessions' | '/more'
  labelKey: string
  icon: Icon
}

/** 手机底部标签栏里的导航标签（spec/web.md § 14.2）。正中的「＋」不是标签，是一个动作，见 `TabBar`。 */
export const TABS: readonly TabDef[] = [
  { id: 'inbox', to: '/inbox', labelKey: 'tabs.inbox', icon: Inbox },
  { id: 'todos', to: '/todos', labelKey: 'tabs.todos', icon: ListChecks },
  { id: 'sessions', to: '/sessions', labelKey: 'tabs.sessions', icon: LayoutList },
  { id: 'more', to: '/more', labelKey: 'tabs.more', icon: Menu },
]

/** 当前路径是哪个标签页的根页面；二级页（会话页、单项详情、节点首页、设置……）不是，它们是全屏的、不显示标签栏。 */
export function tabOfPath(pathname: string): TabId | null {
  const clean = pathname.length > 1 ? pathname.replace(/\/+$/, '') : pathname
  return TABS.find((t) => t.to === clean)?.id ?? null
}

const LINK = 'flex min-h-14 flex-col items-center justify-center gap-0.5 px-1 text-xs font-medium focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent'

/**
 * 手机（< 768px）的主导航，取代左侧抽屉：待处理 / 清单 / ＋ / 会话 / 更多。只在标签页的根页面显示；进二级页（会话页、单项详情）时
 * 由外壳收起、返回时恢复。点目标 ≥ 44px，底边让出安全区。「待处理」上是「现在」的个数。
 * 正中的「＋」是**动作**（打开「记一项」的底部弹层），不是导航目标：渲染成 `<button>`，没有 `aria-current`，可访问名称是「记一项」。
 */
export function TabBar({ current, inboxCount, todoCount = 0 }: { current: TabId | null; inboxCount: number; todoCount?: number }) {
  const { t } = useTranslation()
  const openCapture = useCapture((s) => s.openCapture)
  // 没有任何节点支持清单：不放「清单」和「＋」（点了只会得到一句「不支持」）
  const todoAvailable = useTodoAvailable()
  const tab = (def: TabDef) => {
    const Icon = def.icon
    const label = t(def.labelKey)
    const count = def.id === 'inbox' ? inboxCount : def.id === 'todos' ? todoCount : 0
    const active = current === def.id
    const name = count > 0 ? (def.id === 'inbox' ? t('inbox.navLabel', { count }) : t('todos.navLabel', { count })) : label
    return (
      <li key={def.id} className="min-w-0 flex-1">
        <Link to={def.to} aria-current={active ? 'page' : undefined} aria-label={name} data-testid={`tab-${def.id}`} className={cn(LINK, active ? 'text-accent-text' : 'text-fg-muted')}>
          <span className="relative">
            <Icon className="size-5" aria-hidden />
            {count > 0 ? (
              <span className={cn('absolute -right-2.5 -top-1.5 min-w-4 rounded-full px-1 text-center text-[0.625rem] leading-4 font-semibold', def.id === 'inbox' ? 'bg-warning text-accent-fg' : 'bg-bg-muted text-fg')} data-testid={`tab-${def.id}-count`} aria-hidden>
                {count > 99 ? '99+' : count}
              </span>
            ) : null}
          </span>
          <span className={cn('truncate', active && 'underline decoration-2 underline-offset-4')}>{label}</span>
        </Link>
      </li>
    )
  }
  return (
    <nav aria-label={t('tabs.label')} className="pb-safe shrink-0 border-t border-border bg-bg md:hidden" data-testid="tab-bar">
      <ul className="flex">
        {tab(TABS[0]!)}
        {todoAvailable ? tab(TABS[1]!) : null}
        {todoAvailable ? (
          <li className="min-w-0 flex-1">
            <button type="button" onClick={() => openCapture()} aria-label={t('todos.addTitle')} data-testid="tab-capture" className={cn(LINK, 'w-full')}>
              <span className="grid size-9 place-items-center rounded-full bg-accent text-accent-fg">
                <Plus className="size-5" aria-hidden />
              </span>
            </button>
          </li>
        ) : null}
        {tab(TABS[2]!)}
        {tab(TABS[3]!)}
      </ul>
    </nav>
  )
}
