import type { ReactNode } from 'react'
import { Checkbox, RadioGroup } from 'radix-ui'
import { Check } from 'lucide-react'
import { cn } from './cn'

export interface ChoiceOption<T extends string> {
  value: T
  label: ReactNode
  /** 选项下面的一行小字。 */
  hint?: ReactNode
  disabled?: boolean
  testId?: string
}

/**
 * 单选组（Radix RadioGroup）：每一项是一整行可点的标签，高度 ≥ 36px。
 * 选中状态不只靠颜色：实心圆点 + `data-state`。
 */
export function RadioChoices<T extends string>({ value, onValueChange, options, ariaLabel, name, testId }: { value: T; onValueChange: (v: T) => void; options: ChoiceOption<T>[]; ariaLabel: string; name: string; testId?: string }) {
  return (
    <RadioGroup.Root value={value} onValueChange={(v) => onValueChange(v as T)} aria-label={ariaLabel} name={name} data-testid={testId} className="space-y-1">
      {options.map((o) => {
        const id = `${name}-${o.value}`
        return (
          <div key={o.value} className={cn('flex min-h-9 items-start gap-2.5 rounded-md px-1 py-1.5', o.disabled && 'opacity-50')}>
            <RadioGroup.Item
              id={id}
              value={o.value}
              disabled={o.disabled}
              data-testid={o.testId}
              className="mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-full border border-border-strong bg-bg focus-visible:ring-2 focus-visible:ring-accent/40 data-[state=checked]:border-accent"
            >
              <RadioGroup.Indicator className="block size-2 rounded-full bg-accent" />
            </RadioGroup.Item>
            <label htmlFor={id} className={cn('min-w-0 flex-1 text-sm', o.disabled ? 'cursor-not-allowed' : 'cursor-pointer')}>
              {o.label}
              {o.hint ? <span className="mt-0.5 block text-xs text-fg-subtle">{o.hint}</span> : null}
            </label>
          </div>
        )
      })}
    </RadioGroup.Root>
  )
}

/** 带说明的复选框（Radix Checkbox）。 */
export function CheckboxField({ id, checked, onCheckedChange, label, hint, disabled, testId }: { id: string; checked: boolean; onCheckedChange: (v: boolean) => void; label: ReactNode; hint?: ReactNode; disabled?: boolean; testId?: string }) {
  return (
    <div className={cn('flex min-h-9 items-start gap-2.5 rounded-md px-1 py-1.5', disabled && 'opacity-50')}>
      <Checkbox.Root
        id={id}
        checked={checked}
        disabled={disabled}
        onCheckedChange={(v) => onCheckedChange(v === true)}
        data-testid={testId}
        className="mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-[0.25rem] border border-border-strong bg-bg focus-visible:ring-2 focus-visible:ring-accent/40 data-[state=checked]:border-accent data-[state=checked]:bg-accent"
      >
        <Checkbox.Indicator>
          <Check className="size-3 text-accent-fg" aria-hidden />
        </Checkbox.Indicator>
      </Checkbox.Root>
      <label htmlFor={id} className={cn('min-w-0 flex-1 text-sm', disabled ? 'cursor-not-allowed' : 'cursor-pointer')}>
        {label}
        {hint ? <span className="mt-0.5 block text-xs text-fg-subtle">{hint}</span> : null}
      </label>
    </div>
  )
}
