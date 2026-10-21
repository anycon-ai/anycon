import { useRef, useState, type KeyboardEvent, type PointerEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { clampSidebarWidth, SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN } from '@/stores/prefs'
import { cn } from '@/ui/cn'

/**
 * 宽屏左栏右缘的拖动条：拖动改宽度，双击恢复默认，键盘聚焦后 ← / → 每次 16px（按住 Shift 48px）。
 * 拖动中只调 `onResize`（不写 localStorage），松手才 `onCommit`。
 */
export function SidebarResizeHandle({ width, onResize, onCommit }: { width: number; onResize: (w: number) => void; onCommit: (w: number) => void }) {
  const { t } = useTranslation()
  const drag = useRef<{ x: number; w: number; last: number } | null>(null)
  const [dragging, setDragging] = useState(false)

  const onPointerDown = (e: PointerEvent<HTMLDivElement>): void => {
    if (e.button !== 0) return
    e.preventDefault()
    e.currentTarget.setPointerCapture?.(e.pointerId)
    drag.current = { x: e.clientX, w: width, last: width }
    setDragging(true)
    // 拖动期间整页显示调整光标、不选中文字（index.css 的 html[data-resizing]）
    document.documentElement.dataset.resizing = '1'
  }
  const onPointerMove = (e: PointerEvent<HTMLDivElement>): void => {
    const d = drag.current
    if (!d) return
    d.last = clampSidebarWidth(d.w + e.clientX - d.x)
    onResize(d.last)
  }
  const end = (): void => {
    const d = drag.current
    if (!d) return
    drag.current = null
    setDragging(false)
    delete document.documentElement.dataset.resizing
    onCommit(d.last)
  }
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>): void => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return
    e.preventDefault()
    const step = e.shiftKey ? 48 : 16
    onCommit(clampSidebarWidth(width + (e.key === 'ArrowRight' ? step : -step)))
  }

  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label={t('sidebar.resize')}
      aria-valuemin={SIDEBAR_MIN}
      aria-valuemax={SIDEBAR_MAX}
      aria-valuenow={width}
      tabIndex={0}
      title={t('sidebar.resizeHint')}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onLostPointerCapture={end}
      onDoubleClick={() => onCommit(SIDEBAR_DEFAULT)}
      onKeyDown={onKeyDown}
      data-dragging={dragging || undefined}
      data-testid="sidebar-resize"
      // 8px 宽的热区骑在边框上，悬停 / 拖动 / 聚焦时中间亮一条 2px 的强调色
      className={cn(
        'absolute inset-y-0 -right-1 z-10 w-2 cursor-col-resize touch-none focus-visible:outline-none',
        'after:absolute after:inset-y-0 after:left-1/2 after:w-0.5 after:-translate-x-1/2 after:transition-colors',
        'hover:after:bg-accent/60 focus-visible:after:bg-accent data-[dragging]:after:bg-accent',
      )}
    />
  )
}
