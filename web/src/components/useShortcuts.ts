import { useEffect } from 'react'
import { useNavigate, useParams } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { respondPermission } from '@/features/session/actions'
import { pendingPermissions } from '@/features/session/reducer'
import { handleApproveShortcut, isTypingTarget } from '@/features/session/shortcuts'
import { describeError } from '@/lib/errors'
import { shortNodeId } from '@/lib/ids'
import { useCurrentNode } from '@/lib/nav'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'

function requestCard(requestId: string): Element | null {
  return document.querySelector(`[data-request="${CSS.escape(requestId)}"]`)
}

/**
 * 全局快捷键（spec/web.md § 9）：
 * ⌘. 开合右栏 · ⌘B 开合左栏 · ⌘N 当前项目新建会话 · [ / ] 上 / 下一个会话 · ⌘⇧A / ⌘⇧D 允许 / 拒绝最近一条待审批。
 * ⌘K（命令面板）和 Esc（停止）在各自的组件里。
 */
export function useShortcuts(): void {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { nodeId } = useCurrentNode()
  const nodeOrder = useNodes((x) => x.order)
  const params = useParams({ strict: false }) as { session?: string; project?: string }

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      const mod = e.metaKey || e.ctrlKey
      const key = e.key.toLowerCase()
      const st = useSessions.getState()
      const order = nodeId ? Object.values(st.list).filter((s) => s.node_id === nodeId && !s.archived).sort((a, b) => b.updated_at_ms - a.updated_at_ms) : []

      // 按住不放会不停地触发 keydown：只认第一下（拦下默认行为，但不重复执行）
      if (mod && key === '.') {
        e.preventDefault()
        if (e.repeat) return
        const p = usePrefs.getState()
        p.set({ infoPanelOpen: !p.infoPanelOpen })
        return
      }
      if (mod && !e.shiftKey && !e.altKey && key === 'b') {
        e.preventDefault()
        if (e.repeat) return
        const p = usePrefs.getState()
        p.set({ sidebarOpen: !p.sidebarOpen })
        return
      }
      if (mod && !e.shiftKey && key === 'n' && nodeId) {
        e.preventDefault()
        if (e.repeat) return
        void navigate({ to: '/n/$node/new', params: { node: shortNodeId(nodeId, nodeOrder) }, search: { project: params.project } })
        return
      }
      if (mod && e.shiftKey && (key === 'a' || key === 'd')) {
        const open = params.session && nodeId ? st.open[params.session] : undefined
        if (open && nodeId) {
          // handleApproveShortcut 自己忽略 e.repeat
          handleApproveShortcut(e, {
            pending: pendingPermissions(open.view),
            respond: (p) =>
              respondPermission(nodeId, p).then(
                (outcome) => {
                  // not_found：目录变了（请求已被拒绝、本轮已中止）或请求已不存在——说明一下，不是失败，也不重试
                  if (outcome === 'denied_folder_changed') toast.info(t('permission.deniedFolderChanged'))
                  else if (outcome === 'request_gone') toast.info(t('permission.requestGone'))
                },
                (err: unknown) => toast.error(describeError(err, t)),
              ),
            isVisible: (id) => {
              const r = requestCard(id)?.getBoundingClientRect()
              return !!r && r.bottom > 0 && r.top < window.innerHeight
            },
            reveal: (id) => {
              requestCard(id)?.scrollIntoView({ block: 'center', behavior: 'smooth' })
              toast.info(t('shortcut.reviewFirst'))
            },
          })
        }
        return
      }
      if (!mod && !e.altKey && (e.key === '[' || e.key === ']') && !isTypingTarget(e.target) && nodeId && params.session) {
        const i = order.findIndex((s) => s.id === params.session)
        const next = order[e.key === ']' ? i + 1 : i - 1]
        if (next) {
          e.preventDefault()
          if (e.repeat) return
          void navigate({ to: '/n/$node/s/$session', params: { node: shortNodeId(nodeId, nodeOrder), session: next.id } })
        }
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [navigate, nodeId, nodeOrder, params.project, params.session, t])
}
