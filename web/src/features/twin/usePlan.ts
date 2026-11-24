// 试跑计划：分身页的恢复 / 试跑对话框和节点设置的首次流程共用。
// 只有结果是 `done` 或 `idle` 的试跑才允许「就这么办」；关掉或重来之后迟到的结果不再写回。
import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { TwinRun } from '@/protocol'
import { describeError } from '@/lib/errors'
import { dryRunTwin } from './service'

export type Plan = { kind: 'running' } | { kind: 'ready'; run: TwinRun } | { kind: 'failed'; message: string }

export function usePlan(nodeId: string): { plan: Plan | null; canProceed: boolean; start: () => Promise<void>; reset: () => void } {
  const { t } = useTranslation()
  const [plan, setPlan] = useState<Plan | null>(null)
  const token = useRef(0)
  const start = useCallback(async (): Promise<void> => {
    const mine = ++token.current
    setPlan({ kind: 'running' })
    try {
      const r = await dryRunTwin(nodeId)
      if (token.current === mine) setPlan({ kind: 'ready', run: r.run })
    } catch (e) {
      if (token.current === mine) setPlan({ kind: 'failed', message: describeError(e, t) })
    }
  }, [nodeId, t])
  const reset = useCallback((): void => {
    token.current++
    setPlan(null)
  }, [])
  useEffect(() => () => void token.current++, [])
  const canProceed = plan?.kind === 'ready' && ['done', 'idle'].includes(plan.run.result)
  return { plan, canProceed, start, reset }
}
