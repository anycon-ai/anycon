import type { ReactNode } from 'react'
import { Dialog as RDialog, VisuallyHidden } from 'radix-ui'
import { X } from 'lucide-react'
import { cn } from './cn'

interface Common {
  open: boolean
  onOpenChange: (open: boolean) => void
  title: string
  description?: string
  children: ReactNode
  /** 隐藏可见标题（仍保留给读屏）。 */
  hideTitle?: boolean
  closeLabel?: string
  testId?: string
  /** 对话框宽度：`md`（默认，28rem）或 `lg`（36rem，放目录选择器这类内容）。 */
  size?: 'md' | 'lg'
}

const overlay = 'fixed inset-0 z-40 bg-black/40 backdrop-blur-[1px] data-[state=open]:animate-in'

/** 居中对话框。 */
export function Dialog({ open, onOpenChange, title, description, children, hideTitle, closeLabel = 'Close', testId, size = 'md' }: Common) {
  return (
    <RDialog.Root open={open} onOpenChange={onOpenChange}>
      <RDialog.Portal>
        <RDialog.Overlay className={overlay} />
        <RDialog.Content
          data-testid={testId}
          {...(description ? {} : { 'aria-describedby': undefined })}
          className={cn(
            'fixed left-1/2 top-1/2 z-50 max-h-[85dvh] w-[calc(100vw-2rem)] -translate-x-1/2 -translate-y-1/2 overflow-y-auto rounded-lg border border-border bg-surface p-5 shadow-pop focus:outline-none',
            size === 'lg' ? 'max-w-xl' : 'max-w-md',
          )}
        >
          {hideTitle ? (
            <VisuallyHidden.Root>
              <RDialog.Title>{title}</RDialog.Title>
            </VisuallyHidden.Root>
          ) : (
            <RDialog.Title className="pr-8 text-base font-semibold">{title}</RDialog.Title>
          )}
          {description ? <RDialog.Description className="mt-1 text-sm text-fg-muted">{description}</RDialog.Description> : null}
          <div className="mt-4">{children}</div>
          <RDialog.Close aria-label={closeLabel} className="absolute right-3 top-3 rounded-md p-1.5 text-fg-subtle hover:bg-bg-hover hover:text-fg">
            <X className="size-4" />
          </RDialog.Close>
        </RDialog.Content>
      </RDialog.Portal>
    </RDialog.Root>
  )
}

/** 从侧边滑出的抽屉（左栏 / 右栏）。 */
export function Drawer({ side, open, onOpenChange, title, children, closeLabel = 'Close', className }: Omit<Common, 'description' | 'hideTitle' | 'size'> & { side: 'left' | 'right'; className?: string }) {
  return (
    <RDialog.Root open={open} onOpenChange={onOpenChange}>
      <RDialog.Portal>
        <RDialog.Overlay className={overlay} />
        <RDialog.Content
          aria-describedby={undefined}
          className={cn(
            'fixed inset-y-0 z-50 flex w-[min(22rem,88vw)] flex-col bg-bg shadow-pop focus:outline-none',
            side === 'left' ? 'left-0 border-r border-border' : 'right-0 border-l border-border',
            className,
          )}
        >
          <VisuallyHidden.Root>
            <RDialog.Title>{title}</RDialog.Title>
          </VisuallyHidden.Root>
          <RDialog.Close aria-label={closeLabel} className="absolute right-2 top-2 z-10 rounded-md p-1.5 text-fg-subtle hover:bg-bg-hover hover:text-fg">
            <X className="size-4" />
          </RDialog.Close>
          {children}
        </RDialog.Content>
      </RDialog.Portal>
    </RDialog.Root>
  )
}

/** 底部弹层（手机上的选项面板、操作菜单）。 */
export function Sheet({ open, onOpenChange, title, children, closeLabel = 'Close', testId }: Omit<Common, 'description' | 'hideTitle' | 'size'>) {
  return (
    <RDialog.Root open={open} onOpenChange={onOpenChange}>
      <RDialog.Portal>
        <RDialog.Overlay className={overlay} />
        <RDialog.Content
          data-testid={testId}
          aria-describedby={undefined}
          className="pb-safe fixed inset-x-0 bottom-0 z-50 max-h-[85dvh] overflow-y-auto rounded-t-xl border-t border-border bg-surface p-4 shadow-pop focus:outline-none"
        >
          <div className="mx-auto mb-3 h-1 w-10 rounded-full bg-border-strong" aria-hidden />
          <RDialog.Title className="mb-3 text-sm font-semibold text-fg-muted">{title}</RDialog.Title>
          {children}
          <RDialog.Close className="sr-only">{closeLabel}</RDialog.Close>
        </RDialog.Content>
      </RDialog.Portal>
    </RDialog.Root>
  )
}
