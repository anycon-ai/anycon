import { useEffect, useState } from 'react'
import { ListPlus } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { resolveNodeParam } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { nodeSupportsTodo } from '../store'
import { openCaptureFromMessage, sessionRoute } from './MessageTodoMenu'

interface Sel {
  text: string
  seq: number | undefined
  x: number
  y: number
}

/**
 * 会话页里选中一段文字后浮出的「记到清单」（spec/web.md § 14.5）。只认消息列表里的选区；点它打开输入条（预填选中的文字和回到那条消息的链接），
 * 不会直接记下。`mousedown` 不抢焦点，免得点的时候选区先没了。
 */
export function SelectionCapture() {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const [sel, setSel] = useState<Sel | null>(null)

  useEffect(() => {
    const read = (): void => {
      const container = document.querySelector('[data-testid="message-list"]')
      if (!container) return setSel(null)
      const s = window.getSelection()
      if (!s || s.isCollapsed || s.rangeCount === 0) return setSel(null)
      const text = s.toString().trim()
      const range = s.getRangeAt(0)
      if (!text || !container.contains(range.commonAncestorContainer)) return setSel(null)
      const el = (range.startContainer instanceof Element ? range.startContainer : range.startContainer.parentElement)?.closest('[data-seq]')
      const seq = Number(el?.getAttribute('data-seq'))
      const r = range.getBoundingClientRect()
      // 放在选区下面：系统自己的「复制 / 查询」菜单（iOS）出现在选区上面，不要叠在一起
      setSel({ text, seq: Number.isFinite(seq) && seq > 0 ? seq : undefined, x: r.left + r.width / 2, y: r.bottom })
    }
    // 按选区的位置摆的：一滚动位置就不对了，直接收起（选区还在，再选一下就又出来）
    const hide = (): void => setSel(null)
    document.addEventListener('selectionchange', read)
    window.addEventListener('scroll', hide, true)
    window.addEventListener('resize', hide)
    return () => {
      document.removeEventListener('selectionchange', read)
      window.removeEventListener('scroll', hide, true)
      window.removeEventListener('resize', hide)
    }
  }, [])

  const route = sessionRoute()
  const nodeId = route ? resolveNodeParam(route.node, order) : undefined
  if (!sel || !route || !nodeId || !nodeSupportsTodo(nodeId)) return null
  return (
    <button
      type="button"
      onMouseDown={(e) => e.preventDefault()}
      onClick={() => {
        openCaptureFromMessage(sel.text, route.node, route.session, sel.seq)
        window.getSelection()?.removeAllRanges()
        setSel(null)
      }}
      style={{ position: 'fixed', left: Math.max(8, sel.x - 60), top: sel.y + 8 }}
      className="z-40 inline-flex min-h-9 items-center gap-1.5 rounded-md border border-border bg-surface px-2.5 text-sm font-medium shadow-pop hover:bg-bg-hover"
      data-testid="selection-add-todo"
    >
      <ListPlus className="size-4" aria-hidden />
      {t('todos.addToTodos')}
    </button>
  )
}
