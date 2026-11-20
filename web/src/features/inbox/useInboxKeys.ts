// 「待处理」页自己的键（spec/web.md § 9、§ 4.11）：J / K 上下移动、Enter 把焦点移进详情、⌘⇧A / ⌘⇧D 答复选中的权限请求。
// 单字母键在输入框里、有对话框或菜单开着时都不生效。⌘⇧A / ⌘⇧D 的限制和会话页一样（`handleApproveShortcut`）：
// 只处理 `tool` 类请求、只选一次性选项、卡片不在可见区域时只滚过去、按住不放只算第一下；另外：有对话框或菜单开着不答复、
// 节点不在线不答复（和禁用的按钮一致）、详情刚换到这条请求的头一小会儿（`ARM_DELAY_MS`）不答复。
import { useEffect } from 'react'
import { useTranslation } from 'react-i18next'
import { answerFromShortcut } from '@/features/session/shortcutRespond'
import { handleApproveShortcut, isRequestCardVisible, isTypingTarget, requestCardElement } from '@/features/session/shortcuts'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { permissionBlock } from './block'
import type { InboxItem } from './derive'

/** 有对话框（不含「待处理」自己从右边滑出的详情）、菜单开着：键盘事件是它们的。 */
export function overlayOpen(): boolean {
  return document.querySelector('[role="alertdialog"], [role="menu"], [role="dialog"]:not(.inbox-overlay)') !== null
}

export function useInboxKeys({
  active,
  enter,
  item,
  armed,
  selectedKey,
  move,
  focusDetail,
  choose,
}: {
  active: boolean
  enter: boolean
  /** 详情里正显示的条目。 */
  item: InboxItem | null
  armed: boolean
  selectedKey: string | null
  move: (delta: 1 | -1) => void
  focusDetail: () => void
  choose: (key: string) => void
}): void {
  const { t } = useTranslation()
  useEffect(() => {
    if (!active) return
    const onKey = (e: KeyboardEvent): void => {
      const mod = e.metaKey || e.ctrlKey
      const key = e.key.toLowerCase()
      if (mod && e.shiftKey && (key === 'a' || key === 'd')) {
        if (item?.kind !== 'permission' || !armed || overlayOpen()) return
        if (useNodes.getState().byId[item.nodeId]?.conn !== 'ready') return
        const nodeId = item.nodeId
        // handleApproveShortcut 自己只认 tool 类、只选 allow_once / reject_once、忽略 e.repeat、输入框里不触发
        handleApproveShortcut(e, {
          pending: [permissionBlock(item.request)],
          respond: (p) => void answerFromShortcut(nodeId, p, t, true),
          isVisible: isRequestCardVisible,
          reveal: (id) => {
            requestCardElement(id)?.scrollIntoView({ block: 'center', behavior: 'smooth' })
            toast.info(t('shortcut.reviewFirst'))
          },
        })
        return
      }
      if (mod || e.altKey || e.shiftKey || isTypingTarget(e.target) || overlayOpen()) return
      if (key === 'j' || key === 'k') {
        e.preventDefault()
        move(key === 'j' ? 1 : -1)
        return
      }
      if (e.key === 'Enter' && enter && !e.repeat) {
        // 焦点在某一行的主按钮上（或没有焦点）：选中它并把焦点移进详情。在别的按钮、链接上的 Enter 照常触发它们
        const target = e.target as HTMLElement | null
        const row = target?.closest?.('[data-inbox-row]')
        const onRowButton = !!row && !!target?.closest('[data-inbox-focus]')
        if (!onRowButton && target !== document.body) return
        const rowKey = row?.getAttribute('data-inbox-row') ?? selectedKey
        if (!rowKey) return
        e.preventDefault()
        choose(rowKey)
        window.setTimeout(focusDetail, 30)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [active, enter, item, armed, selectedKey, move, focusDetail, choose, t])
}
