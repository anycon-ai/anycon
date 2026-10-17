import { useState, type ReactNode } from 'react'
import { Switch as RSwitch, Tabs as RTabs, ToggleGroup } from 'radix-ui'
import { Check, Copy, Loader2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { copyText } from '@/lib/clipboard'
import { toast } from '@/stores/ui'
import { cn } from './cn'

export type Tone = 'neutral' | 'accent' | 'success' | 'warning' | 'danger' | 'info'

/** 状态色的底色 + 文字色（chip 和自定义的胶囊共用）。 */
export const tones: Record<Tone, string> = {
  neutral: 'bg-bg-muted text-fg-muted',
  accent: 'bg-accent-subtle text-accent-text',
  success: 'bg-success-subtle text-success',
  warning: 'bg-warning-subtle text-warning',
  danger: 'bg-danger-subtle text-danger',
  info: 'bg-info-subtle text-info',
}

export function Chip({ tone = 'neutral', children, className, testId, title, icon, dataState }: { tone?: Tone; children: ReactNode; className?: string; testId?: string; title?: string; icon?: ReactNode; dataState?: string }) {
  return (
    <span data-testid={testId} data-state={dataState} title={title} className={cn('inline-flex max-w-full items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium whitespace-nowrap', tones[tone], className)}>
      {icon}
      <span className="truncate">{children}</span>
    </span>
  )
}

export function Dot({ tone = 'neutral', pulse, className }: { tone?: Tone; pulse?: boolean; className?: string }) {
  const color: Record<Tone, string> = {
    neutral: 'bg-fg-subtle',
    accent: 'bg-accent',
    success: 'bg-success',
    warning: 'bg-warning',
    danger: 'bg-danger',
    info: 'bg-info',
  }
  return <span aria-hidden className={cn('inline-block size-2 shrink-0 rounded-full', color[tone], pulse && 'animate-pulse-soft', className)} />
}

export function Spinner({ className }: { className?: string }) {
  return <Loader2 aria-hidden className={cn('size-4 animate-spin text-fg-subtle', className)} />
}

export function Skeleton({ className }: { className?: string }) {
  return <div aria-hidden className={cn('animate-pulse-soft rounded-md bg-bg-muted', className)} />
}

export function Card({ children, className, testId, tone }: { children: ReactNode; className?: string; testId?: string; tone?: 'danger' | 'warning' | 'accent' }) {
  return (
    <div
      data-testid={testId}
      className={cn(
        'rounded-lg border bg-surface',
        tone === 'danger' ? 'border-danger/50' : tone === 'warning' ? 'border-warning/50' : tone === 'accent' ? 'border-accent/50' : 'border-border',
        className,
      )}
    >
      {children}
    </div>
  )
}

export function EmptyState({ icon, title, children, action }: { icon?: ReactNode; title: string; children?: ReactNode; action?: ReactNode }) {
  return (
    <div className="mx-auto flex max-w-sm flex-col items-center gap-3 px-4 py-12 text-center">
      {icon ? <div className="text-fg-subtle">{icon}</div> : null}
      <h2 className="text-base font-semibold">{title}</h2>
      {children ? <p className="text-sm text-fg-muted">{children}</p> : null}
      {action}
    </div>
  )
}

/** `iconOnly`：只显示图标（可访问名称和悬停提示仍是 label）。 */
export function CopyButton({ text, label, className, iconOnly, testId }: { text: string; label?: string; className?: string; iconOnly?: boolean; testId?: string }) {
  const { t } = useTranslation()
  const [done, setDone] = useState(false)
  return (
    <button
      type="button"
      aria-label={label ?? t('common.copy')}
      title={iconOnly ? (label ?? t('common.copy')) : undefined}
      data-testid={testId}
      data-copied={done}
      className={cn('inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-xs text-fg-subtle hover:bg-bg-hover hover:text-fg', className)}
      onClick={() => {
        const fail = (e?: unknown): void => {
          console.warn('anycon: could not write to the clipboard', e)
          toast.error(t('common.copyFailed'))
        }
        copyText(text).then(() => {
          setDone(true)
          setTimeout(() => setDone(false), 1500)
        }, fail)
      }}
    >
      {done ? <Check className="size-3.5 text-success" /> : <Copy className="size-3.5" />}
      {iconOnly ? null : done ? t('common.copied') : (label ?? t('common.copy'))}
    </button>
  )
}

export function Switch({ checked, onCheckedChange, label, disabled, id, testId }: { checked: boolean; onCheckedChange: (v: boolean) => void; label: string; disabled?: boolean; id?: string; testId?: string }) {
  return (
    <RSwitch.Root
      id={id}
      data-testid={testId}
      aria-label={label}
      checked={checked}
      disabled={disabled}
      onCheckedChange={onCheckedChange}
      className="relative h-5 w-9 shrink-0 rounded-full bg-border-strong transition-colors data-[state=checked]:bg-accent disabled:opacity-50"
    >
      <RSwitch.Thumb className="block size-4 translate-x-0.5 rounded-full bg-white shadow transition-transform data-[state=checked]:translate-x-[1.125rem]" />
    </RSwitch.Root>
  )
}

export interface SegmentedOption<T extends string> {
  value: T
  label: ReactNode
  disabled?: boolean
  testId?: string
  title?: string
}

/** 分段控件（思考等级、主题等）。再次点击当前项不会取消选择。 */
export function Segmented<T extends string>({ value, onChange, options, ariaLabel, size = 'md', disabled, testId, fill }: { value: T | null; onChange: (v: T) => void; options: SegmentedOption<T>[]; ariaLabel: string; size?: 'sm' | 'md'; disabled?: boolean; testId?: string; fill?: boolean }) {
  return (
    <ToggleGroup.Root
      type="single"
      data-testid={testId}
      aria-label={ariaLabel}
      value={value ?? ''}
      disabled={disabled}
      onValueChange={(v) => {
        if (v) onChange(v as T)
      }}
      className={cn('max-w-full rounded-md border border-border-strong bg-bg-subtle p-0.5', fill ? 'flex' : 'inline-flex overflow-x-auto')}
    >
      {options.map((o) => (
        <ToggleGroup.Item
          key={o.value}
          value={o.value}
          disabled={o.disabled}
          data-testid={o.testId}
          title={o.title}
          className={cn(
            'rounded-[0.3rem] font-medium text-fg-muted transition-colors hover:text-fg disabled:opacity-40 data-[state=on]:bg-surface data-[state=on]:text-fg data-[state=on]:shadow-sm',
            // fill：撑满一行、各项等宽，窄屏上文字换行而不是横向滚动
            fill ? 'min-w-0 flex-1 px-2 text-center leading-tight' : 'whitespace-nowrap',
            size === 'sm' ? 'px-2 py-1 text-xs' : fill ? 'py-1.5 text-[0.8125rem]' : 'px-3 py-1.5 text-[0.8125rem]',
          )}
        >
          {o.label}
        </ToggleGroup.Item>
      ))}
    </ToggleGroup.Root>
  )
}

export interface TabItem {
  value: string
  label: ReactNode
  content: ReactNode
  testId?: string
}

export function Tabs({ items, value, onValueChange, defaultValue }: { items: TabItem[]; value?: string; onValueChange?: (v: string) => void; defaultValue?: string }) {
  return (
    <RTabs.Root value={value} onValueChange={onValueChange} defaultValue={defaultValue ?? items[0]?.value} className="flex min-h-0 flex-col">
      <RTabs.List className="scroll-thin flex shrink-0 gap-1 overflow-x-auto border-b border-border">
        {items.map((it) => (
          <RTabs.Trigger
            key={it.value}
            value={it.value}
            data-testid={it.testId}
            className="-mb-px border-b-2 border-transparent px-3 py-2 text-sm font-medium whitespace-nowrap text-fg-muted hover:text-fg data-[state=active]:border-accent data-[state=active]:text-fg"
          >
            {it.label}
          </RTabs.Trigger>
        ))}
      </RTabs.List>
      {items.map((it) => (
        <RTabs.Content key={it.value} value={it.value} className="min-h-0 pt-4 focus:outline-none">
          {it.content}
        </RTabs.Content>
      ))}
    </RTabs.Root>
  )
}

export function SectionTitle({ children, action }: { children: ReactNode; action?: ReactNode }) {
  return (
    <div className="mb-2 flex items-center justify-between gap-2">
      <h2 className="text-xs font-semibold tracking-wide text-fg-subtle uppercase">{children}</h2>
      {action}
    </div>
  )
}
