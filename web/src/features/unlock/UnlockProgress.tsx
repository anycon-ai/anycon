import { Check, Circle } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { EnrollStage } from '@/transport/enroll'
import { cn } from '@/ui/cn'
import { Spinner } from '@/ui/Misc'
import type { UnlockEntry } from './errors'
import type { UnlockPhase } from './useUnlock'

const ORDER: EnrollStage[] = ['connecting', 'starting', 'waiting', 'code', 'deriving', 'verifying', 'handshaking', 'confirming', 'finishing', 'done']

interface Step {
  key: 'connecting' | 'starting' | 'waiting' | 'verifying' | 'handshaking' | 'confirming'
  /** 这一步对应的传输层阶段。 */
  stages: EnrollStage[]
}

// 连接 hub → 发起接入 →（等待批准）→ 验证 → 握手 → 与节点确认 → 完成（spec/web.md § 4.3.1）
const STEPS: Step[] = [
  { key: 'connecting', stages: ['connecting'] },
  { key: 'starting', stages: ['starting'] },
  { key: 'waiting', stages: ['waiting', 'code'] },
  { key: 'verifying', stages: ['deriving', 'verifying'] },
  { key: 'handshaking', stages: ['handshaking'] },
  { key: 'confirming', stages: ['confirming', 'finishing'] },
]

function status(phase: EnrollStage, step: Step): 'done' | 'active' | 'todo' {
  const p = ORDER.indexOf(phase)
  if (phase === 'done' || p > Math.max(...step.stages.map((s) => ORDER.indexOf(s)))) return 'done'
  return step.stages.includes(phase) ? 'active' : 'todo'
}

export function UnlockProgress({ entry, phase }: { entry: UnlockEntry; phase: Exclude<UnlockPhase, 'idle' | 'error'> }) {
  const { t } = useTranslation()
  // 只有扫码和请求批准要等人；接入码和口令直接验证
  const steps = STEPS.filter((s) => s.key !== 'waiting' || entry === 'scan' || entry === 'approve')
  const label = (s: Step): string => {
    if (s.key === 'verifying') return phase === 'deriving' ? t('unlock.steps.deriving') : t('unlock.steps.verifying')
    if (s.key === 'waiting') return phase === 'code' ? t('unlock.steps.enterCode') : t(entry === 'scan' ? 'unlock.steps.waitingScan' : 'unlock.steps.waitingApproval')
    return t(`unlock.steps.${s.key}`)
  }
  return (
    <ol className="space-y-2 rounded-md border border-border bg-bg-subtle p-3 text-sm" data-testid="unlock-progress" data-phase={phase}>
      {steps.map((s) => {
        const st = status(phase, s)
        return (
          <li key={s.key} className="flex items-center gap-2" data-step={s.key} data-status={st}>
            {st === 'done' ? <Check className="size-4 text-success" aria-hidden /> : st === 'active' ? <Spinner className="text-accent-text" /> : <Circle className="size-4 text-fg-subtle" aria-hidden />}
            <span className={cn(st === 'todo' && 'text-fg-subtle')}>{label(s)}</span>
          </li>
        )
      })}
    </ol>
  )
}
