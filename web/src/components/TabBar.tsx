import type { ComponentType } from 'react'
import { Link } from '@tanstack/react-router'
import { Inbox, LayoutList, Menu } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { cn } from '@/ui/cn'

export type TabId = 'inbox' | 'sessions' | 'more'

interface TabDef {
  id: TabId
  to: '/inbox' | '/sessions' | '/more'
  labelKey: string
  icon: ComponentType<{ className?: string; 'aria-hidden'?: boolean }>
}

/**
 * 手机底部标签栏的位置（spec/web.md § 14.2）。现在是待处理 / 会话 / 更多；最终是 待处理 / 清单 / ＋ / 会话 / 更多，清单和正中的「＋」
 * 到时插在「待处理」和「会话」之间，做完之前不渲染（不放点了没反应的入口）。
 */
export const TABS: readonly TabDef[] = [
  { id: 'inbox', to: '/inbox', labelKey: 'tabs.inbox', icon: Inbox },
  { id: 'sessions', to: '/sessions', labelKey: 'tabs.sessions', icon: LayoutList },
  { id: 'more', to: '/more', labelKey: 'tabs.more', icon: Menu },
]

/** 当前路径是哪个标签页的根页面；二级页（会话页、节点首页、设置……）不是，它们是全屏的、不显示标签栏。 */
export function tabOfPath(pathname: string): TabId | null {
  const clean = pathname.length > 1 ? pathname.replace(/\/+$/, '') : pathname
  return TABS.find((t) => t.to === clean)?.id ?? null
}

/**
 * 手机（< 768px）的主导航，取代左侧抽屉。只在标签页的根页面显示；进二级页（会话页、节点首页、设置）时由外壳收起、
 * 返回时恢复。点目标 ≥ 44px，底边让出安全区。「待处理」上是「现在」的个数。
 */
export function TabBar({ current, inboxCount }: { current: TabId | null; inboxCount: number }) {
  const { t } = useTranslation()
  return (
    <nav aria-label={t('tabs.label')} className="pb-safe shrink-0 border-t border-border bg-bg md:hidden" data-testid="tab-bar">
      <ul className="flex">
        {TABS.map((tab) => {
          const Icon = tab.icon
          const label = t(tab.labelKey)
          const count = tab.id === 'inbox' ? inboxCount : 0
          const active = current === tab.id
          return (
            <li key={tab.id} className="min-w-0 flex-1">
              <Link
                to={tab.to}
                aria-current={active ? 'page' : undefined}
                // 可访问名称以可见的文字开头，数字跟在后面（读屏不只是读一个数）
                aria-label={count > 0 ? t('inbox.navLabel', { count }) : label}
                data-testid={`tab-${tab.id}`}
                className={cn('flex min-h-14 flex-col items-center justify-center gap-0.5 px-2 text-xs font-medium focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent', active ? 'text-accent-text' : 'text-fg-muted')}
              >
                <span className="relative">
                  <Icon className="size-5" aria-hidden />
                  {count > 0 ? (
                    <span className="absolute -right-2.5 -top-1.5 min-w-4 rounded-full bg-warning px-1 text-center text-[0.625rem] leading-4 font-semibold text-accent-fg" data-testid="tab-inbox-count" aria-hidden>
                      {count > 99 ? '99+' : count}
                    </span>
                  ) : null}
                </span>
                <span className={cn('truncate', active && 'underline decoration-2 underline-offset-4')}>{label}</span>
              </Link>
            </li>
          )
        })}
      </ul>
    </nav>
  )
}
