// 规则写入与同步拒绝的本地化说明，保留用户可以据此处理的原因。
import type { TFunction } from 'i18next'
import type { RuleFail } from '../service'
import type { RuleTextError } from '../validate'

/** 一次写失败 → 一句话（`over_budget` 带着字节数和上限，说明怎么办）。 */
export function failText(t: TFunction, f: RuleFail): string {
  switch (f.kind) {
    case 'offline':
      return t('rules.errors.offline')
    case 'changed':
      return f.current ? t('rules.errors.changed') : t('rules.errors.changedGone')
    case 'over_budget':
      return f.bytes !== undefined && f.cap !== undefined ? t('rules.errors.overBudget', { bytes: f.bytes, cap: f.cap }) : t('rules.errors.overBudgetPlain')
    case 'too_many':
      return t('rules.errors.tooMany')
    case 'too_many_pending':
      return t('rules.errors.tooManyPending')
    case 'not_syncable':
      return t('rules.group.notSyncable')
    case 'invisible':
      return t('rules.errors.invisible')
    case 'invalid':
      return f.field === 'project' ? t('rules.errors.project') : f.field === 'text' ? t('rules.errors.text') : t('rules.errors.rejected')
    case 'not_found':
      return t('rules.errors.notFound')
    case 'forbidden':
      return t('rules.errors.forbidden')
    case 'unsupported':
      return t('rules.errors.unsupported')
    case 'link':
      return t('rules.errors.link')
    case 'failed':
      return t('rules.errors.failed')
  }
}

/** 本地校验的错误 → 一句话。 */
export function textErrorText(t: TFunction, e: RuleTextError): string {
  return t(`rules.errors.local.${e}`)
}

/** 同步队列里的拒绝沿用规则编辑器的说明；未知原因仍显示节点给出的稳定标识。 */
export function syncErrorText(t: TFunction, error: { code: string; reason?: string; field?: string } | undefined): string {
  const reason = error?.reason ?? error?.code ?? 'unknown'
  let fail: RuleFail
  switch (reason) {
    case 'over_budget': fail = { kind: 'over_budget' }; break
    case 'too_many': fail = { kind: 'too_many' }; break
    case 'not_syncable': fail = { kind: 'not_syncable' }; break
    case 'forbidden': fail = { kind: 'forbidden' }; break
    case 'unsupported': fail = { kind: 'unsupported' }; break
    default: fail = { kind: 'invalid', field: error?.field }
  }
  return t('rules.group.failure', { reason: `${failText(t, fail)} (${reason})`, field: error?.field ?? '' })
}
