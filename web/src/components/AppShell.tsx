import { useEffect, useRef, useState } from 'react'
import { Outlet, useNavigate, useParams, useRouterState } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { shortNodeId } from '@/lib/ids'
import { useCurrentNode } from '@/lib/nav'
import { useIsWide } from '@/lib/useMediaQuery'
import { useSessions } from '@/stores/sessions'
import { useNodes } from '@/stores/nodes'
import { clampSidebarWidth, usePrefs } from '@/stores/prefs'
import { useUi } from '@/stores/ui'
import { Drawer } from '@/ui/Overlay'
import { EnrollApprovalDialog } from '@/features/approve/EnrollApprovalDialog'
import { ScanApproveDialog } from '@/features/approve/ScanApproveDialog'
import { ApprovalBanner } from './ApprovalBanner'
import { useUnsavedDraftsGuard } from '@/features/todos/drafts'
import { useRunningMoveGuard } from '@/features/todos/move'
import { CaptureHost } from './CaptureHost'
import { ManageSpacesDialog } from './ManageSpacesDialog'
import { HomeNotice } from './HomeNotice'
import { FollowUpRuleHost } from '@/features/rules/ui/AlwaysDoThis'
import { InboxAnnouncer } from './InboxAnnouncer'
import { CommandPalette } from './CommandPalette'
import { NewProjectHost } from './NewProjectHost'
import { keptPageOf, PhonePages } from './PageHosts'
import { Sidebar, SidebarRail } from './Sidebar'
import { SidebarResizeHandle } from './SidebarResizeHandle'
import { StatusBanner } from './StatusBanner'
import { useShortcuts } from './useShortcuts'
import { useSidebarPeek } from './useSidebarPeek'
import { cn } from '@/ui/cn'

/**
 * 已登录页面的外壳：左栏 + 状态横幅 + 页面。
 * 左栏 ≥768px 时钉住就常驻占位、可拖动调宽；没钉住时只留一条窄条，悬停浮出、盖在页面上，移开收回。
 * 手机（< 768px）上左栏是抽屉：顶栏左边的菜单按钮打开，里面是和桌面同一个左栏，点了去处就收起（spec/web.md § 3.3）。
 */
export function AppShell() {
  const { t } = useTranslation()
  const { nodeId } = useCurrentNode()
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const lastNode = usePrefs((s) => s.lastNode)
  const setPrefs = usePrefs((s) => s.set)
  const sidebarOpen = usePrefs((s) => s.sidebarOpen)
  const savedWidth = usePrefs((s) => s.sidebarWidth)
  // 拖动中的宽度只放在本地 state，松手才写进偏好
  const [dragWidth, setDragWidth] = useState<number | null>(null)
  const sidebarWidth = dragWidth ?? clampSidebarWidth(savedWidth)
  const palette = useUi((s) => s.paletteOpen)
  const setPalette = useUi((s) => s.setPalette)
  const wide = useIsWide()
  const pathname = useRouterState({ select: (s) => s.location.pathname })
  const kept = keptPageOf(pathname)
  // 地址带着 `?node=&session=` 的 `/inbox`：从通知点进来的，升级说明先不弹
  const deepLink = useRouterState({ select: (s) => s.location.pathname === '/inbox' && !!(s.location.search as { node?: string; session?: string } | undefined)?.node })
  const drawerOpen = useUi((s) => s.leftDrawerOpen)
  const setDrawer = useUi((s) => s.setLeftDrawer)
  useUnsavedDraftsGuard()
  useRunningMoveGuard()
  const setLastSessionsPath = useUi((s) => s.setLastSessionsPath)
  const flyout = useRef<HTMLElement>(null)
  const { peek, close: closePeek, railProps } = useSidebarPeek(!sidebarOpen, flyout)
  const togglePin = (): void => setPrefs({ sidebarOpen: !sidebarOpen })
  const search = (): void => {
    closePeek()
    setPalette(true)
  }
  useShortcuts()

  const activeNode = nodeId ?? (lastNode && byId[lastNode] ? lastNode : order[0])

  // 换到宽屏时抽屉没有意义了，关掉它（不然窄回来时它还开着）
  useEffect(() => {
    if (wide) setDrawer(false)
  }, [wide, setDrawer])

  useEffect(() => {
    if (nodeId && nodeId !== lastNode) setPrefs({ lastNode: nodeId })
  }, [nodeId, lastNode, setPrefs])

  // `G` 然后 `S` 回到这里：最近一次在「会话」里（不含节点设置）打开的页面
  useEffect(() => {
    if (/^\/n\/[^/]+(\/(?!settings(\/|$)).*)?$/.test(pathname)) setLastSessionsPath(pathname)
  }, [pathname, setLastSessionsPath])

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        if (e.repeat) return // 按住不放不要让面板一开一合
        setPalette(!useUi.getState().paletteOpen)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [setPalette])

  return (
    <div className="flex h-screen-dvh overflow-hidden bg-bg">
      {/* z-30：浮出的左栏盖住页面，但在菜单 / 对话框（z-40 起）下面 */}
      {wide ? (
        <div className="relative z-30 flex shrink-0" data-testid="sidebar-dock">
          {sidebarOpen ? null : <SidebarRail nodeId={activeNode} onSearch={search} onPin={togglePin} {...railProps} />}
          {/* 最宽不超过半个窗口，窄窗口里中栏不会被挤没。没钉住时从窄条后面滑出，收回时只是藏起来（不卸载），
              从左栏里打开的对话框、项目的展开状态都还在 */}
          <aside
            ref={flyout}
            className={cn(
              'max-w-[50vw] shrink-0 border-r border-border',
              sidebarOpen
                ? 'relative'
                : cn(
                    'absolute inset-y-0 left-full shadow-pop transition-[translate,visibility] duration-200 ease-out motion-reduce:transition-none',
                    peek ? 'visible translate-x-0' : 'invisible -translate-x-full',
                  ),
            )}
            style={{ width: sidebarWidth }}
            data-testid="sidebar-aside"
            data-state={sidebarOpen ? 'pinned' : peek ? 'peek' : 'hidden'}
          >
            <Sidebar nodeId={activeNode} onNavigate={sidebarOpen ? undefined : closePeek} onSearch={search} pin={{ pinned: sidebarOpen, onToggle: togglePin }} />
            <SidebarResizeHandle
              width={sidebarWidth}
              onResize={setDragWidth}
              onCommit={(w) => {
                setDragWidth(null)
                setPrefs({ sidebarWidth: w })
              }}
            />
          </aside>
        </div>
      ) : (
        <Drawer side="left" open={drawerOpen} onOpenChange={setDrawer} title={t('sidebar.menu')} closeLabel={t('common.close')}>
          <Sidebar
            nodeId={activeNode}
            onNavigate={() => setDrawer(false)}
            onSearch={() => {
              setDrawer(false)
              setPalette(true)
            }}
          />
        </Drawer>
      )}
      <div className="relative flex min-w-0 flex-1 flex-col">
        {wide ? <CaptureHost /> : null}
        <StatusBannerForNode nodeId={nodeId} />
        {/* 手机上「待处理」「清单」常驻在这里（绝对定位叠在页面区上，去别的页面时只是藏起来）；其余页面照常走 Outlet */}
        <div className="relative flex min-h-0 flex-1 flex-col">
          {wide ? null : <PhonePages />}
          {wide || !kept ? <Outlet /> : null}
        </div>
        <PendingApprovalBanner nodeId={nodeId} />
      </div>
      <CommandPalette open={palette} onOpenChange={setPalette} />
      {wide ? null : <CaptureHost />}
      <InboxAnnouncer />
      <HomeNotice deferred={deepLink} />
      <NewProjectHost />
      <FollowUpRuleHost />
      <ManageSpacesDialog />
      {/* 别的设备请求接入时的批准对话框、应用内扫码（ADR-36）：任何页面都能打开 */}
      <EnrollApprovalDialog />
      <ScanApproveDialog />
    </div>
  )
}

function StatusBannerForNode({ nodeId }: { nodeId: string | undefined }) {
  const conn = useNodes((s) => (nodeId ? s.byId[nodeId]?.conn : undefined))
  return <StatusBanner nodeId={nodeId ?? null} nodeConn={conn ?? null} />
}

/**
 * 手机上不在会话页时的待审批横幅（会话页自己有，见 SessionPage）：提示有会话在等审批，点「查看」直达那个会话。
 * ≥768px 左栏常驻，待审批会话在左栏置顶，用不着（横幅自带 md:hidden）。
 */
function PendingApprovalBanner({ nodeId }: { nodeId: string | undefined }) {
  const navigate = useNavigate()
  const order = useNodes((s) => s.order)
  const pending = useSessions((s) => s.pending)
  const list = useSessions((s) => s.list)
  const params = useParams({ strict: false }) as { session?: string }
  if (!nodeId || params.session) return null
  const waiting = [...new Set(Object.values(pending).filter((p) => p.node_id === nodeId).map((p) => p.session_id))]
  if (waiting.length === 0) return null
  const first = waiting[0]!
  return (
    <ApprovalBanner
      count={waiting.length}
      title={list[first]?.title || first}
      onReview={() => void navigate({ to: '/n/$node/s/$session', params: { node: shortNodeId(nodeId, order), session: first } })}
    />
  )
}
