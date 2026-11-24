// 判断用量的显示：有花费就写花费，只有 token 数就写 token 数，都没有就不显示。
import type { TFunction } from 'i18next'
import type { JudgeUsage } from '@/protocol'
import { compactNumber, formatCost } from '@/lib/format'

export function usageText(t: TFunction, usage: JudgeUsage | null | undefined): string | null {
  if (!usage) return null
  if (typeof usage.cost_usd === 'number') return formatCost(usage.cost_usd)
  if (typeof usage.input_tokens === 'number' || typeof usage.output_tokens === 'number') {
    return t('twin.today.costTokens', { input: compactNumber(usage.input_tokens ?? 0), output: compactNumber(usage.output_tokens ?? 0) })
  }
  return null
}

/** 把多天的用量加起来：只要有一天报了花费，就按花费加；token 数同理。 */
export function sumUsage(list: ReadonlyArray<JudgeUsage | null | undefined>): JudgeUsage | null {
  const sum: JudgeUsage = {}
  for (const u of list) {
    if (!u) continue
    for (const key of ['cost_usd', 'input_tokens', 'output_tokens'] as const) {
      const n = u[key]
      if (typeof n === 'number') sum[key] = (sum[key] ?? 0) + n
    }
  }
  return Object.keys(sum).length ? sum : null
}
