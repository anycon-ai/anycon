// 同组规则同步的期望状态与持久化记录；跨节点只比较内容，不比较各自的时间戳。
import type { Rule } from '@/protocol'

export type DesiredRule = { text: string; status: 'active' | 'disabled' | 'rejected'; sync: 'group' }
export interface RuleSyncRecord {
  seq?: number
  node: string
  rule_id: string
  desired: DesiredRule | 'deleted'
  state: 'pending' | 'failed'
  error?: { code: string; field?: string; reason?: string }
  attempts?: number
  drops?: number
  claimed_at?: number
  claimed_by?: string
}

export function desiredRule(rule: Rule): DesiredRule | null {
  if (rule.project || rule.sync !== 'group' || !['active', 'disabled', 'rejected'].includes(rule.status)) return null
  return { text: rule.text, status: rule.status as DesiredRule['status'], sync: 'group' }
}
