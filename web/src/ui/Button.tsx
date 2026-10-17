import type { ButtonHTMLAttributes, ReactNode, Ref } from 'react'
import { Loader2 } from 'lucide-react'
import { cn } from './cn'

export type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger' | 'outline' | 'outline-danger' | 'outline-success' | 'success'
export type ButtonSize = 'sm' | 'md' | 'lg' | 'icon' | 'icon-sm'

const variants: Record<ButtonVariant, string> = {
  primary: 'bg-accent text-accent-fg hover:bg-accent-hover disabled:opacity-50',
  secondary: 'bg-bg-muted text-fg hover:bg-border disabled:opacity-50',
  ghost: 'text-fg-muted hover:bg-bg-hover hover:text-fg disabled:opacity-40',
  outline: 'border border-border-strong text-fg hover:bg-bg-hover disabled:opacity-50',
  'outline-danger': 'border border-danger/50 text-danger hover:bg-danger-subtle disabled:opacity-50',
  'outline-success': 'border border-success/60 text-success hover:bg-success-subtle disabled:opacity-50',
  danger: 'bg-danger text-white hover:opacity-90 disabled:opacity-50',
  success: 'bg-success text-white hover:opacity-90 disabled:opacity-50',
}
const sizes: Record<ButtonSize, string> = {
  sm: 'h-8 px-3 text-[0.8125rem] gap-1.5',
  md: 'h-9 px-4 text-sm gap-2',
  lg: 'h-11 px-5 text-base gap-2',
  icon: 'h-9 w-9 justify-center',
  'icon-sm': 'h-7 w-7 justify-center',
}

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant
  size?: ButtonSize
  loading?: boolean
  icon?: ReactNode
  ref?: Ref<HTMLButtonElement>
}

export function Button({ variant = 'secondary', size = 'md', loading, icon, className, children, disabled, type = 'button', ref, ...rest }: ButtonProps) {
  return (
    <button
      ref={ref}
      type={type}
      disabled={disabled || loading}
      className={cn(
        'inline-flex shrink-0 select-none items-center rounded-md font-medium whitespace-nowrap transition-colors',
        variants[variant],
        sizes[size],
        className,
      )}
      {...rest}
    >
      {loading ? <Loader2 className="size-4 animate-spin" aria-hidden /> : icon}
      {children}
    </button>
  )
}
