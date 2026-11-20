// 手机三个标签页（待处理 / 会话 / 更多）的承载（spec/web.md § 14.2）。
// 标签之间切换要保留各自的滚动位置和没发出去的输入（比如提问卡片里填了一半的回答），所以手机上三个页面一旦打开过就常驻在外壳里，
// 切走时只是藏起来（`invisible` + `inert`：保留布局盒，滚动位置不丢；读屏和键盘都够不到）。进二级页（会话页等）时它们同样藏着。
// 桌面和平板的路由直接渲染页面本身；`/sessions`、`/more` 只给手机用，宽屏上回到「待处理」。
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import { Navigate, useNavigate, useRouterState, useSearch } from '@tanstack/react-router'
import { InboxPage } from '@/pages/InboxPage'
import { MorePage } from '@/pages/MorePage'
import { SessionsPage } from '@/pages/SessionsPage'
import { useIsWide } from '@/lib/useMediaQuery'
import { cn } from '@/ui/cn'
import { TABS, tabOfPath, type TabId } from './TabBar'

export function usePathname(): string {
  return useRouterState({ select: (s) => s.location.pathname })
}

/** `/inbox`：地址里带 `?node=&session=` 是从通知点进来的，页面定位那一条后把它们去掉（replace，不多一条历史记录）。 */
export function InboxHost() {
  const pathname = usePathname()
  const search = useSearch({ strict: false }) as { node?: unknown; session?: unknown }
  const navigate = useNavigate()
  const active = pathname === '/inbox'
  const node = typeof search.node === 'string' ? search.node : undefined
  const session = typeof search.session === 'string' ? search.session : undefined
  const locate = active && node && session ? { node, session } : undefined
  const done = useCallback(() => void navigate({ to: '/inbox', search: {}, replace: true }), [navigate])
  return <InboxPage active={active} locate={locate} onLocateDone={done} />
}

function PageOf({ tab }: { tab: TabId }): ReactNode {
  switch (tab) {
    case 'inbox':
      return <InboxHost />
    case 'sessions':
      return <SessionsPage />
    case 'more':
      return <MorePage />
  }
}

/** 标签页的路由组件：手机上页面由外壳常驻（见 `PhoneTabs`），这里什么都不画。 */
export function TabRoute({ tab }: { tab: TabId }) {
  const wide = useIsWide()
  if (!wide) return null
  if (tab === 'inbox') return <InboxHost />
  return <Navigate to="/inbox" replace />
}

/** 手机上常驻的标签页。没打开过的不挂载。 */
export function PhoneTabs() {
  const current = tabOfPath(usePathname())
  const [visited, setVisited] = useState<ReadonlySet<TabId>>(new Set())
  useEffect(() => {
    if (current) setVisited((v) => (v.has(current) ? v : new Set(v).add(current)))
  }, [current])
  return (
    <>
      {TABS.map((tab) => {
        if (tab.id !== current && !visited.has(tab.id)) return null
        const shown = tab.id === current
        return (
          <div key={tab.id} data-testid={`tab-pane-${tab.id}`} data-active={shown ? 'true' : undefined} inert={!shown} className={cn('absolute inset-0 flex min-h-0 flex-col bg-bg', !shown && 'invisible')}>
            <PageOf tab={tab.id} />
          </div>
        )
      })}
    </>
  )
}
