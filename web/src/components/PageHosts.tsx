// 「待处理」和清单的路由承载（spec/web.md § 3.3、§ 4.11、§ 4.12）。
// 手机上这两页一旦打开过就常驻在外壳里（`PhonePages`），去别的页面时只是藏起来（`invisible` + `inert`：保留布局盒，滚动位置不丢；
// 读屏和键盘都够不到）。所以从「待处理」进会话页再回来，滚动位置、没发出去的输入（提问卡片里填了一半的回答）、
// 「在别处处理了」的占位都还在，也不会替用户再默认展开一条。桌面和平板的路由直接渲染页面本身。
// `/sessions`、`/more` 是以前手机底部标签栏的页面，现在它们的内容都在左栏（手机上是抽屉）里，这两个地址回到「待处理」。
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import { Navigate, useNavigate, useRouterState, useSearch } from '@tanstack/react-router'
import { InboxPage } from '@/pages/InboxPage'
import { TodoDetailPage } from '@/pages/TodoDetailPage'
import { TodosPage } from '@/pages/TodosPage'
import { useIsWide } from '@/lib/useMediaQuery'
import { cn } from '@/ui/cn'

/** 手机上常驻的页面。 */
export type KeptPage = 'inbox' | 'todos'
const KEPT: readonly { id: KeptPage; path: string }[] = [
  { id: 'inbox', path: '/inbox' },
  { id: 'todos', path: '/todos' },
]

/** 当前路径是不是常驻页面本身（不含下一级页：单项详情、验收……）。 */
export function keptPageOf(pathname: string): KeptPage | null {
  const clean = pathname.length > 1 ? pathname.replace(/\/+$/, '') : pathname
  return KEPT.find((p) => p.path === clean)?.id ?? null
}

function usePathname(): string {
  return useRouterState({ select: (s) => s.location.pathname })
}

/** `/inbox`：地址里带 `?node=&session=` 是从通知点进来的，页面定位那一条后把它们去掉（replace，不多一条历史记录）。 */
function InboxHost() {
  const pathname = usePathname()
  const search = useSearch({ strict: false }) as { node?: unknown; session?: unknown; todo?: unknown }
  const navigate = useNavigate()
  const active = keptPageOf(pathname) === 'inbox'
  const node = typeof search.node === 'string' ? search.node : undefined
  const session = typeof search.session === 'string' ? search.session : undefined
  const todo = typeof search.todo === 'string' ? search.todo : undefined
  const locate = active && node && (session || todo) ? { node, session, ...(todo ? { todo } : {}) } : undefined
  const done = useCallback(() => void navigate({ to: '/inbox', search: {}, replace: true }), [navigate])
  return <InboxPage active={active} locate={locate} onLocateDone={done} />
}

/** `/todos`：手机上常驻，藏在后面时不处理键。 */
function TodosHost() {
  return <TodosPage active={keptPageOf(usePathname()) === 'todos'} />
}

function PageOf({ page }: { page: KeptPage }): ReactNode {
  return page === 'inbox' ? <InboxHost /> : <TodosHost />
}

/** `/inbox`、`/todos` 的路由组件：手机上页面由外壳常驻（见 `PhonePages`），这里什么都不画。 */
export function KeptRoute({ page }: { page: KeptPage }) {
  const wide = useIsWide()
  if (!wide) return null
  return <PageOf page={page} />
}

/** `/todos/:node/:todo`：宽屏在清单页右半边显示详情；手机是全屏的下一级页。 */
export function TodoRoute({ node, todo }: { node: string; todo: string }) {
  const wide = useIsWide()
  return wide ? <TodosPage selected={{ node, todo }} /> : <TodoDetailPage node={node} todo={todo} />
}

/** 以前的手机标签页地址（`/sessions`、`/more`）：回到「待处理」。 */
export function ToInbox() {
  return <Navigate to="/inbox" replace />
}

/** 手机上常驻的页面。没打开过的不挂载。 */
export function PhonePages() {
  const current = keptPageOf(usePathname())
  const [visited, setVisited] = useState<ReadonlySet<KeptPage>>(new Set())
  useEffect(() => {
    if (current) setVisited((v) => (v.has(current) ? v : new Set(v).add(current)))
  }, [current])
  return (
    <>
      {KEPT.map(({ id }) => {
        if (id !== current && !visited.has(id)) return null
        const shown = id === current
        return (
          <div key={id} data-testid={`kept-page-${id}`} data-active={shown ? 'true' : undefined} inert={!shown} className={cn('absolute inset-0 flex min-h-0 flex-col bg-bg', !shown && 'invisible')}>
            <PageOf page={id} />
          </div>
        )
      })}
    </>
  )
}
