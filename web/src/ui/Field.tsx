import type { InputHTMLAttributes, ReactNode, Ref, SelectHTMLAttributes, TextareaHTMLAttributes } from 'react'
import { cn } from './cn'

const base =
  'rounded-md border border-border-strong bg-bg px-3 text-sm text-fg placeholder:text-fg-subtle focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:opacity-60'

/**
 * 缺省占满一行；调用方的 `className` 里给了宽度（`w-44`、`w-auto`……）就不再带 `w-full`。
 * `cn` 不合并冲突的类，而 Tailwind 生成的 CSS 里 `w-full` 排在其他 `w-*` 后面：两个都在时调用方给的宽度永远不生效。
 */
function widthOf(className: string | undefined): string | undefined {
  return className && /(^|\s)w-/.test(className) ? undefined : 'w-full'
}

export function Input({ className, ref, ...rest }: InputHTMLAttributes<HTMLInputElement> & { ref?: Ref<HTMLInputElement> }) {
  return <input ref={ref} className={cn(base, widthOf(className), 'h-9', className)} {...rest} />
}

export function Textarea({ className, ref, ...rest }: TextareaHTMLAttributes<HTMLTextAreaElement> & { ref?: Ref<HTMLTextAreaElement> }) {
  return <textarea ref={ref} className={cn(base, widthOf(className), 'py-2', className)} {...rest} />
}

export function Select({ className, children, ref, ...rest }: SelectHTMLAttributes<HTMLSelectElement> & { ref?: Ref<HTMLSelectElement> }) {
  return (
    <select ref={ref} className={cn(base, widthOf(className), 'h-9 pr-8', className)} {...rest}>
      {children}
    </select>
  )
}

export function Field({ label, hint, error, children, htmlFor }: { label: ReactNode; hint?: ReactNode; error?: ReactNode; children: ReactNode; htmlFor?: string }) {
  return (
    <div className="space-y-1.5">
      <label htmlFor={htmlFor} className="block text-[0.8125rem] font-medium text-fg">
        {label}
      </label>
      {children}
      {hint && !error ? <p className="text-xs text-fg-subtle">{hint}</p> : null}
      {error ? <p className="text-xs text-danger">{error}</p> : null}
    </div>
  )
}
