import { useEffect, useRef } from 'react'
import { X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useCapture } from '@/features/todos/capture'
import { overlayOpen } from '@/features/inbox/useInboxKeys'
import { CaptureForm } from '@/features/todos/ui/CaptureForm'
import { useIsWide } from '@/lib/useMediaQuery'
import { Sheet } from '@/ui/Overlay'

/**
 * 「记一项」的输入条（spec/web.md § 14.5）：手机上是底部弹层（输入框已聚焦、键盘已弹起），桌面是页面顶部的一行输入条——
 * 在内容列的最上面占一行，不遮当前页面，`Esc` 关掉。两种样子里是同一个表单。`/capture` 页面里直接内联同一个表单，不经过这里。
 */
export function CaptureHost() {
  const { t } = useTranslation()
  const open = useCapture((s) => s.open)
  const close = useCapture((s) => s.close)
  const wide = useIsWide()
  const opener = useRef<Element | null>(null)
  useEffect(() => {
    if (open) opener.current = document.activeElement
    else if (opener.current instanceof HTMLElement && document.activeElement === document.body) opener.current.focus()
  }, [open])

  // 桌面的输入条：不管焦点在哪里，开着的时候 Esc 关掉（有对话框 / 菜单开着时 Esc 先属于它们）
  useEffect(() => {
    if (!open || !wide) return
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape' && !e.defaultPrevented && !overlayOpen()) close()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open, wide, close])

  if (!open) return null
  if (!wide) {
    return (
      <Sheet open onOpenChange={(o) => !o && close()} title={t('todos.addTitle')} closeLabel={t('common.close')} testId="capture-sheet">
        <CaptureForm />
      </Sheet>
    )
  }
  return (
    <div className="absolute inset-x-0 top-0 z-30 border-b border-border bg-bg-subtle px-3 py-2 shadow-pop" role="region" aria-label={t('todos.addTitle')} data-testid="capture-bar">
      <div className="flex items-start gap-2">
        <div className="min-w-0 flex-1">
          <CaptureForm onEscape={close} compact />
        </div>
        <button type="button" onClick={close} aria-label={t('common.close')} className="grid size-9 shrink-0 place-items-center rounded-md text-fg-subtle hover:bg-bg-hover" data-testid="capture-close">
          <X className="size-4" aria-hidden />
        </button>
      </div>
    </div>
  )
}
