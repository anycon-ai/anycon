import { Input } from '@/ui/Field'
import { cn } from '@/ui/cn'

/** `123456` → `123 456`。 */
function group(digits: string): string {
  return digits.length > 3 ? `${digits.slice(0, 3)} ${digits.slice(3)}` : digits
}

/**
 * 6 位数字码输入框（接入码）：只收数字，粘贴带空格 / 连字符的也行，输满 6 位调用 `onComplete`（自动提交）。
 * `inputmode=numeric` + `autocomplete=one-time-code`：手机键盘直接是数字键盘，也能从短信 / 密码管理器填。
 */
export function CodeInput({
  id,
  value,
  onChange,
  onComplete,
  disabled,
  invalid,
  label,
  testId,
  autoFocus,
}: {
  /** 配了就能被 `<label htmlFor>` 指到（登录页 / 绑定邮箱的字段标签用）。 */
  id?: string
  value: string
  onChange: (digits: string) => void
  onComplete?: (digits: string) => void
  disabled?: boolean
  invalid?: boolean
  label: string
  testId?: string
  autoFocus?: boolean
}) {
  return (
    <Input
      id={id}
      data-testid={testId}
      aria-label={label}
      aria-invalid={invalid ? true : undefined}
      inputMode="numeric"
      autoComplete="one-time-code"
      autoCapitalize="off"
      autoCorrect="off"
      spellCheck={false}
      autoFocus={autoFocus}
      disabled={disabled}
      maxLength={14}
      placeholder="000 000"
      value={group(value)}
      onChange={(e) => {
        const digits = e.target.value.replace(/\D/g, '').slice(0, 6)
        onChange(digits)
        if (digits.length === 6 && value.length < 6) onComplete?.(digits)
      }}
      className={cn('h-12 text-center font-mono text-2xl tracking-[0.25em]', invalid && 'border-danger')}
    />
  )
}
