import { useRef, useState, type PointerEvent } from 'react'
import { Bot, Check, CircleDashed, MoreHorizontal, RotateCcw, Trash2, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { formatDateTime } from '@/lib/format'
import { Chip } from '@/ui/Misc'
import { Menu, MenuContent, MenuItem, MenuSeparator, MenuTrigger } from '@/ui/Menu'
import { cn } from '@/ui/cn'
import { LONG_PRESS_MS, LONG_PRESS_SLOP, swipeOffset, swipeVerdict } from '../swipe'
import { isClosed, type ViewTodo } from '../types'
import { cleanOneLine } from '../validate'

export interface TodoRowProps {
  item: ViewTodo
  /** 勾了多个节点时标出节点名。 */
  nodeName?: string
  /** 这个节点现在连得上：连不上时「未同步」的标记才有意义（在线时队列几乎是立刻发完的）。 */
  online: boolean
  selected: boolean
  multi: boolean
  checked: boolean
  onOpen: () => void
  onToggleCheck: () => void
  onDone: (undo: boolean) => void
  onReopen: () => void
  onDrop: () => void
  onDelete: () => void
}

/**
 * 清单页的一行。点一下打开详情；手机上向左滑 = 完成（5 秒撤销），长按（和右边的「⋯」按钮）= 同样的动作菜单：每个滑动动作都有按钮。
 * 多选（`X`）时行首是勾选框。标题是用户 / agent 的文字，一律按文本显示（去掉控制字符和双向控制符）。
 */
export function TodoRow({ item, nodeName, online, selected, multi, checked, onOpen, onToggleCheck, onDone, onReopen, onDrop, onDelete }: TodoRowProps) {
  const { t, i18n } = useTranslation()
  const [dx, setDx] = useState(0)
  const [menu, setMenu] = useState(false)
  const start = useRef<{ x: number; y: number; timer: ReturnType<typeof setTimeout> | null; id: number } | null>(null)
  const closed = isClosed(item.state)
  const agent = item.source.kind === 'agent'

  const clearTimer = (): void => {
    if (start.current?.timer) clearTimeout(start.current.timer)
    if (start.current) start.current.timer = null
  }
  const down = (e: PointerEvent): void => {
    if (e.pointerType !== 'touch') return
    const timer = setTimeout(() => {
      start.current = null
      setDx(0)
      setMenu(true)
    }, LONG_PRESS_MS)
    start.current = { x: e.clientX, y: e.clientY, timer, id: e.pointerId }
  }
  const move = (e: PointerEvent): void => {
    const s = start.current
    if (!s || s.id !== e.pointerId) return
    const mx = e.clientX - s.x
    const my = e.clientY - s.y
    if (Math.hypot(mx, my) > LONG_PRESS_SLOP) clearTimer()
    setDx(swipeOffset(mx, my))
  }
  const up = (e: PointerEvent): void => {
    const s = start.current
    clearTimer()
    start.current = null
    if (!s || s.id !== e.pointerId) return
    const v = swipeVerdict(e.clientX - s.x, e.clientY - s.y, e.pointerType)
    setDx(0)
    if (v === 'done' && !closed) onDone(true)
  }
  const cancel = (): void => {
    clearTimer()
    start.current = null
    setDx(0)
  }

  const meta = [item.project, nodeName, item.due_ms ? t('todos.dueAt', { time: formatDateTime(item.due_ms, i18n.language) }) : undefined].filter(Boolean).join(' · ')
  return (
    <li className="relative overflow-hidden rounded-lg border border-border bg-surface" data-testid="todo-row" data-id={item.id} data-state={item.state} data-selected={selected ? 'true' : undefined}>
      <div className="flex items-stretch" style={{ transform: dx ? `translateX(${dx}px)` : undefined, touchAction: 'pan-y' }} onPointerDown={down} onPointerMove={move} onPointerUp={up} onPointerCancel={cancel} onContextMenu={(e) => { if ((e.nativeEvent as unknown as { pointerType?: string }).pointerType === 'touch') e.preventDefault() }}>
        {multi ? (
          <label className="grid min-h-11 w-11 shrink-0 cursor-pointer place-items-center">
            <input type="checkbox" checked={checked} onChange={onToggleCheck} aria-label={t('todos.select', { title: cleanOneLine(item.title, 60) })} className="size-4 accent-[var(--accent)]" data-testid="todo-check" />
          </label>
        ) : null}
        <button
          type="button"
          data-todo-focus
          onClick={multi ? onToggleCheck : onOpen}
          aria-current={selected ? 'true' : undefined}
          className={cn('flex min-h-11 min-w-0 flex-1 items-start gap-2 rounded-lg px-3 py-2.5 text-left hover:bg-bg-hover', selected && 'bg-accent-subtle')}
          data-testid="todo-main"
        >
          <span className={cn('min-w-0 flex-1', closed && 'text-fg-subtle')}>
            <span className={cn('block truncate font-medium [overflow-wrap:anywhere]', closed && 'line-through')}>{cleanOneLine(item.title)}</span>
            {meta ? <span className="mt-0.5 block truncate text-xs text-fg-subtle">{meta}</span> : null}
          </span>
          {agent ? (
            <Chip tone="info" icon={<Bot className="size-3" aria-hidden />} testId="todo-agent">
              {t('todos.byAgent')}
            </Chip>
          ) : null}
          {item.failed ? (
            <Chip tone="danger" testId="todo-failed">
              {t('todos.notSaved')}
            </Chip>
          ) : item.unsynced && !online ? (
            <Chip tone="warning" icon={<CircleDashed className="size-3" aria-hidden />} testId="todo-unsynced">
              {t('todos.notSynced')}
            </Chip>
          ) : null}
        </button>
        <Menu open={menu} onOpenChange={setMenu}>
          <MenuTrigger asChild>
            <button type="button" aria-label={t('todos.actions', { title: cleanOneLine(item.title, 60) })} className="grid min-h-11 w-11 shrink-0 place-items-center text-fg-subtle hover:text-fg" data-testid="todo-menu">
              <MoreHorizontal className="size-4" aria-hidden />
            </button>
          </MenuTrigger>
          <MenuContent align="end" className="w-52">
            <MenuItem icon={<Check className="size-4" />} onSelect={onOpen} testId="todo-act-open">
              {t('todos.open')}
            </MenuItem>
            {closed ? (
              <MenuItem icon={<RotateCcw className="size-4" />} onSelect={onReopen} testId="todo-act-reopen">
                {t('todos.reopen')}
              </MenuItem>
            ) : (
              <>
                <MenuItem icon={<Check className="size-4" />} onSelect={() => onDone(true)} testId="todo-act-done">
                  {t('todos.done')}
                </MenuItem>
                <MenuItem icon={<X className="size-4" />} onSelect={onDrop} testId="todo-act-drop">
                  {t('todos.drop')}
                </MenuItem>
              </>
            )}
            <MenuSeparator />
            <MenuItem icon={<Trash2 className="size-4" />} danger onSelect={onDelete} testId="todo-act-delete">
              {t('todos.delete')}
            </MenuItem>
          </MenuContent>
        </Menu>
      </div>
    </li>
  )
}
