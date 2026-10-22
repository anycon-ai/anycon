import { useEffect, useState } from 'react'
import { Outlet, useNavigate, useParams } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { shortNodeId } from '@/lib/ids'
import { useCurrentNode } from '@/lib/nav'
import { useSessions } from '@/stores/sessions'
import { useNodes } from '@/stores/nodes'
import { clampSidebarWidth, usePrefs } from '@/stores/prefs'
import { useUi } from '@/stores/ui'
import { Drawer } from '@/ui/Overlay'
import { EnrollApprovalDialog } from '@/features/approve/EnrollApprovalDialog'
import { ScanApproveDialog } from '@/features/approve/ScanApproveDialog'
import { ApprovalBanner } from './ApprovalBanner'
import { CommandPalette } from './CommandPalette'
import { Sidebar } from './Sidebar'
import { SidebarResizeHandle } from './SidebarResizeHandle'
import { StatusBanner } from './StatusBanner'
import { useShortcuts } from './useShortcuts'
import { cn } from '@/ui/cn'

/** 已登录页面的外壳：左栏（≥768px 常驻、可收起、可拖动调宽，手机是抽屉）+ 状态横幅 + 页面。 */
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
  const open = useUi((s) => s.leftDrawerOpen)
  const setOpen = useUi((s) => s.setLeftDrawer)
  const [palette, setPalette] = useState(false)
  useShortcuts()

  const activeNode = nodeId ?? (lastNode && byId[lastNode] ? lastNode : order[0])

  useEffect(() => {
    if (nodeId && nodeId !== lastNode) setPrefs({ lastNode: nodeId })
  }, [nodeId, lastNode, setPrefs])

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        if (e.repeat) return // 按住不放不要让面板一开一合
        setPalette((v) => !v)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  return (
    <div className="flex h-screen-dvh overflow-hidden bg-bg">
      {/* 最宽不超过半个窗口，窄窗口里中栏不会被挤没 */}
      <aside className={cn('relative hidden max-w-[50vw] shrink-0 border-r border-border', sidebarOpen && 'md:block')} style={{ width: sidebarWidth }} data-testid="sidebar-aside">
        <Sidebar nodeId={activeNode} onSearch={() => setPalette(true)} onCollapse={() => setPrefs({ sidebarOpen: false })} />
        <SidebarResizeHandle
          width={sidebarWidth}
          onResize={setDragWidth}
          onCommit={(w) => {
            setDragWidth(null)
            setPrefs({ sidebarWidth: w })
          }}
        />
      </aside>
      <Drawer side="left" open={open} onOpenChange={setOpen} title={t('sidebar.menu')} closeLabel={t('common.close')}>
        <Sidebar nodeId={activeNode} onNavigate={() => setOpen(false)} onSearch={() => { setOpen(false); setPalette(true) }} />
      </Drawer>
      <div className="flex min-w-0 flex-1 flex-col">
        <StatusBannerForNode nodeId={nodeId} />
        <div className="flex min-h-0 flex-1 flex-col">
          <Outlet />
        </div>
        <PendingApprovalBanner nodeId={nodeId} />
      </div>
      <CommandPalette open={palette} onOpenChange={setPalette} />
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
