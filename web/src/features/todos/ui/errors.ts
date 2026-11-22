import type { TFunction } from 'i18next'
import type { OutboxError } from '../types'
import type { FieldError } from '../validate'

/** 本地校验的错误 → 一句话。 */
export function fieldErrorText(t: TFunction, e: FieldError): string {
  return t(`todos.errors.${e.field}.${e.reason}`)
}

/** daemon 拒绝一条改动的原因（`invalid` + `data.field` / `data.reason`）→ 一句话。 */
export function outboxErrorText(t: TFunction, e: OutboxError): string {
  if (e.code === 'link') return t('todos.errors.link')
  if (e.reason === 'repeat_template') return t('todos.errors.repeat.template')
  if (e.code === 'forbidden' && e.field === 'repeat') return t('todos.errors.repeat.rejected')
  if (e.reason === 'too_many') return t('todos.errors.full')
  if (e.reason === 'too_many_unreviewed') return t('todos.errors.fullUnreviewed')
  if (e.field === 'title' || e.field === 'body' || e.field === 'project' || e.field === 'due_ms' || e.field === 'repeat') return t(`todos.errors.${e.field}.rejected`)
  return t('todos.errors.rejected')
}
