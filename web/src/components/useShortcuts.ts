import { useEffect, useRef } from 'react'
import { useNavigate, useParams } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { pendingPermissions } from '@/features/session/reducer'
import { answerFromShortcut } from '@/features/session/shortcutRespond'
import { handleApproveShortcut, isRequestCardVisible, isTypingTarget, requestCardElement } from '@/features/session/shortcuts'
import { shortNodeId } from '@/lib/ids'
import { useCurrentNode } from '@/lib/nav'
import { navigateTo } from '@/lib/navigate'
import { useCapture } from '@/features/todos/capture'
import { useTodoAvailable } from '@/features/todos/store'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { toast, useUi } from '@/stores/ui'
import { overlayOpen } from '@/features/inbox/useInboxKeys'

/** `G` 之后等多久还认第二个键（`G` 然后 `I` / `S`）。 */
export const GO_TIMEOUT_MS = 1500

/**
 * 全局快捷键（spec/web.md § 9）：
 * ⌘. 开合右栏 · ⌘B 开合左栏 · ⌘N 当前项目新建会话 · [ / ] 上 / 下一个会话 · ⌘⇧A / ⌘⇧D 允许 / 拒绝最近一条待审批 ·
 * `G` 然后 `I` / `T` / `S` 去「待处理」/「清单」/「会话」· `C` 记一项（单字母键在输入框里、有对话框或菜单开着时都不生效）。
 * ⌘K（命令面板）和 Esc（停止）在各自的组件里；「待处理」页自己的 J / K / Enter / ⌘⇧A / ⌘⇧D 在 `features/inbox/useInboxKeys.ts`。
 */
export function useShortcuts(): void {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { nodeId } = useCurrentNode()
  const nodeOrder = useNodes((x) => x.order)
  const params = useParams({ strict: false }) as { session?: string; project?: string }
  // 按过 `G` 的时刻（毫秒）；0 = 没有在等第二个键
  const goAt = useRef(0)
  // `C`、`G` `T` 只在有节点支持清单时生效（没有入口就没有快捷键）
  const todoAvailable = useRef(false)
  todoAvailable.current = useTodoAvailable()

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      const mod = e.metaKey || e.ctrlKey
      const key = e.key.toLowerCase()

      // `G` 然后 `I` / `S`：先按 G，GO_TIMEOUT_MS 内再按 I（待处理）或 S（会话，回到上次的会话）；别的键取消
      const waiting = goAt.current !== 0 && Date.now() - goAt.current < GO_TIMEOUT_MS
      if (e.key.length === 1 && !['Shift', 'Meta', 'Control', 'Alt'].includes(e.key)) goAt.current = 0
      if (!mod && !e.altKey && !e.shiftKey && !e.repeat && !isTypingTarget(e.target) && !overlayOpen()) {
        if (key === 'g') {
          goAt.current = Date.now()
          return
        }
        if (key === 'c' && todoAvailable.current) {
          // 记一项：任何页面一个键，输入，回车；不离开当前页面（spec/web.md § 14.5）
          e.preventDefault()
          useCapture.getState().openCapture()
          return
        }
        if (waiting && key === 't' && todoAvailable.current) {
          e.preventDefault()
          void navigate({ to: '/todos' })
          return
        }
        if (waiting && key === 'i') {
          e.preventDefault()
          void navigate({ to: '/inbox' })
          return
        }
        if (waiting && key === 's') {
          e.preventDefault()
          const last = useUi.getState().lastSessionsPath
          if (last) navigateTo(last)
          else {
            const nodes = useNodes.getState()
            const node = usePrefs.getState().lastNode
            const target = node && nodes.byId[node] ? node : nodes.order[0]
            if (target) navigateTo(`/n/${shortNodeId(target, nodes.order)}`)
          }
          return
        }
      }
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
            respond: (p) => answerFromShortcut(nodeId, p, t),
            isVisible: isRequestCardVisible,
            reveal: (id) => {
              requestCardElement(id)?.scrollIntoView({ block: 'center', behavior: 'smooth' })
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
