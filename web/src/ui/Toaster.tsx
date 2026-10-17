import { X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useUi } from '@/stores/ui'
import { cn } from './cn'

export function Toaster() {
  const { t } = useTranslation()
  const toasts = useUi((s) => s.toasts)
  const dismiss = useUi((s) => s.dismiss)
  return (
    <div aria-live="polite" className="pointer-events-none fixed inset-x-0 top-0 z-[60] flex flex-col items-center gap-2 p-3 pt-[max(0.75rem,env(safe-area-inset-top))]">
      {toasts.map((x) => (
        <div
          key={x.id}
          role={x.kind === 'error' ? 'alert' : 'status'}
          className={cn(
            'pointer-events-auto flex max-w-md items-start gap-2 rounded-lg border px-3 py-2 text-sm shadow-pop',
            x.kind === 'error' ? 'border-danger/50 bg-danger-subtle text-danger' : x.kind === 'success' ? 'border-success/50 bg-success-subtle text-success' : 'border-border bg-surface text-fg',
          )}
        >
          <span className="min-w-0 flex-1">{x.text}</span>
          {x.action ? (
            <button
              type="button"
              className="shrink-0 rounded px-1.5 py-0.5 text-xs font-medium underline underline-offset-2 hover:bg-bg-hover"
              onClick={() => {
                dismiss(x.id)
                x.action?.run()
              }}
            >
              {x.action.label}
            </button>
          ) : null}
          <button type="button" aria-label={t('common.dismiss')} onClick={() => dismiss(x.id)} className="rounded p-0.5 opacity-60 hover:opacity-100">
            <X className="size-3.5" />
          </button>
        </div>
      ))}
    </div>
  )
}
