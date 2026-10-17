import type { ReactNode } from 'react'
import { DropdownMenu } from 'radix-ui'
import { Check } from 'lucide-react'
import { cn } from './cn'

export const Menu = DropdownMenu.Root
export const MenuTrigger = DropdownMenu.Trigger

export function MenuContent({ children, className, align = 'start' }: { children: ReactNode; className?: string; align?: 'start' | 'end' | 'center' }) {
  return (
    <DropdownMenu.Portal>
      <DropdownMenu.Content
        align={align}
        sideOffset={6}
        collisionPadding={8}
        className={cn('z-50 max-h-[min(24rem,70dvh)] min-w-48 max-w-[min(22rem,92vw)] overflow-y-auto rounded-lg border border-border bg-surface p-1 shadow-pop', className)}
      >
        {children}
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  )
}

const itemCls =
  'relative flex cursor-pointer select-none items-start gap-2 rounded-md px-2.5 py-2 text-sm outline-none data-[disabled]:cursor-not-allowed data-[disabled]:opacity-50 data-[highlighted]:bg-bg-hover'

export function MenuItem({ children, onSelect, danger, disabled, testId, icon, hint }: { children: ReactNode; onSelect?: () => void; danger?: boolean; disabled?: boolean; testId?: string; icon?: ReactNode; hint?: string }) {
  return (
    <DropdownMenu.Item data-testid={testId} disabled={disabled} onSelect={onSelect} className={cn(itemCls, danger && 'text-danger')}>
      {icon}
      <span className="min-w-0 flex-1">
        {children}
        {hint ? <span className="mt-0.5 block text-xs font-normal text-fg-subtle">{hint}</span> : null}
      </span>
    </DropdownMenu.Item>
  )
}

export function MenuSeparator() {
  return <DropdownMenu.Separator className="my-1 h-px bg-border" />
}

export function MenuLabel({ children }: { children: ReactNode }) {
  return <DropdownMenu.Label className="px-2.5 py-1.5 text-xs font-medium text-fg-subtle">{children}</DropdownMenu.Label>
}

export const MenuRadioGroup = DropdownMenu.RadioGroup

export function MenuRadioItem({ value, children, disabled, testId }: { value: string; children: ReactNode; disabled?: boolean; testId?: string }) {
  return (
    <DropdownMenu.RadioItem value={value} disabled={disabled} data-testid={testId} className={cn(itemCls, 'pl-8')}>
      <DropdownMenu.ItemIndicator className="absolute left-2.5 top-2.5">
        <Check className="size-4 text-accent-text" />
      </DropdownMenu.ItemIndicator>
      <span className="min-w-0 flex-1">{children}</span>
    </DropdownMenu.RadioItem>
  )
}
