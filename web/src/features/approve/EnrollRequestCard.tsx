import { useState } from 'react'
import { Check, Clock, ShieldAlert, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { describeError } from '@/lib/errors'
import { formatCountdown, localExpiry } from '@/lib/expiry'
import { formatRelative } from '@/lib/format'
import { displayHint } from '@/lib/hint'
import { useNow } from '@/lib/useNow'
import { enrollKey, useEnroll, type EnrollItem, type EnrollResolution } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { approveCode, denyEnroll, groupCode, isAlreadyResolved, isGone, refreshEnrolls } from './actions'

type Local =
  | { kind: 'idle' }
  | { kind: 'busy'; action: 'approve' | 'deny' }
  /** 我们批准了它：6 位码只在这里（组件状态），不进 store。 */
  | { kind: 'approved'; code: string; expiresAt: number }
  | { kind: 'denied' }
  | { kind: 'taken' }
  | { kind: 'gone' }

/** 谁处理的：认得出的设备显示名，认不出的是「另一台设备」。 */
function useDeviceLabel(nodeId: string): (id: string | null | undefined) => string {
  const { t } = useTranslation()
  const self = useHub((s) => s.snapshot.deviceId)
  const devices = useNodes((s) => s.byId[nodeId]?.devices)
  return (id) => {
    if (!id) return t('approve.anotherDevice')
    if (id === self) return t('approve.thisDevice')
    if (id === 'local') return t('security.local')
    return devices?.find((d) => d.device_id === id)?.name ?? t('approve.anotherDevice')
  }
}

/**
 * 一条接入请求的批准卡片（批准对话框和节点设置的「待批准的接入请求」共用，spec/web.md § 4.3.2）。
 * 设备信息是新设备自报的、没有经过认证：写明「只批准你眼前这台设备」。批准后的 6 位码只显示在这里，关掉就看不到了。
 */
export function EnrollRequestCard({ nodeId, enrollId, onScan }: { nodeId: string; enrollId: string; onScan?: () => void }) {
  const { t, i18n } = useTranslation()
  const key = enrollKey(nodeId, enrollId)
  const item: EnrollItem | undefined = useEnroll((s) => s.items[key])
  const resolution: EnrollResolution | undefined = useEnroll((s) => s.resolved[key])
  const nodeName = useNodes((s) => s.byId[nodeId]?.name) ?? ''
  const who = useDeviceLabel(nodeId)
  const self = useHub((s) => s.snapshot.deviceId)
  const [local, setLocal] = useState<Local>({ kind: 'idle' })
  const now = useNow(1000, true)

  const run = async (action: 'approve' | 'deny'): Promise<void> => {
    setLocal({ kind: 'busy', action })
    try {
      if (action === 'approve') {
        const res = await approveCode(nodeId, enrollId)
        if (!res.code) throw new Error('the daemon did not return a code')
        setLocal({ kind: 'approved', code: res.code, expiresAt: localExpiry(res.expires_at_ms, Date.now()) })
      } else {
        await denyEnroll(nodeId, enrollId)
        setLocal({ kind: 'denied' })
        useEnroll.getState().remove(nodeId, enrollId)
      }
    } catch (e) {
      if (isAlreadyResolved(e)) {
        // 别的设备先处理了：以 daemon 的列表为准，收起成一行
        setLocal({ kind: 'taken' })
        void refreshEnrolls(nodeId)
      } else if (isGone(e)) {
        setLocal({ kind: 'gone' })
        void refreshEnrolls(nodeId)
      } else {
        setLocal({ kind: 'idle' })
        toast.error(describeError(e, t))
      }
    }
  }

  const line = (text: string, tone: 'muted' | 'success' | 'danger' = 'muted') => (
    <p className={tone === 'success' ? 'flex items-center gap-2 text-sm text-success' : tone === 'danger' ? 'flex items-center gap-2 text-sm text-danger' : 'flex items-center gap-2 text-sm text-fg-muted'} role="status" data-testid="approve-resolved">
      {tone === 'success' ? <Check className="size-4 shrink-0" aria-hidden /> : tone === 'danger' ? <X className="size-4 shrink-0" aria-hidden /> : <Clock className="size-4 shrink-0" aria-hidden />}
      {text}
    </p>
  )

  // ---- 我们批准了：显示 6 位码（只在这个组件的状态里）----
  if (local.kind === 'approved' && !(resolution && resolution.result !== 'approved')) {
    const left = local.expiresAt - now
    if (resolution?.result === 'completed') return line(t('approve.completed'), 'success')
    if (left <= 0) return line(t('approve.codeExpired'))
    return (
      <div className="space-y-3 text-center" data-testid="approve-approved">
        <p className="text-sm text-fg-muted">{t('approve.codeIntro', { node: nodeName })}</p>
        <p className="font-mono text-5xl font-semibold tracking-[0.2em]" data-testid="approve-code" aria-label={local.code.split('').join(' ')}>
          {groupCode(local.code)}
        </p>
        <p className="text-xs text-fg-subtle" data-testid="approve-code-countdown">
          {t('approve.expiresIn', { time: formatCountdown(left) })}
        </p>
        <p className="flex items-start justify-center gap-2 rounded-md border border-warning/40 bg-warning-subtle p-2 text-left text-sm text-warning">
          <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
          {t('approve.codeWarning')}
        </p>
      </div>
    )
  }

  // ---- 已经有结果了 ----
  if (resolution) {
    const byMe = resolution.byDevice === self
    switch (resolution.result) {
      case 'approved':
        return line(byMe ? t('approve.approvedByMe') : t('approve.approvedBy', { who: who(resolution.byDevice) }), 'success')
      case 'denied':
        return line(byMe ? t('approve.deniedByMe') : t('approve.deniedBy', { who: who(resolution.byDevice) }), 'danger')
      case 'completed':
        return line(t('approve.completed'), 'success')
      case 'expired':
        return line(t('approve.expired'))
      case 'failed':
        return line(t('approve.failed'), 'danger')
    }
  }
  if (local.kind === 'denied') return line(t('approve.deniedByMe'), 'danger')
  if (local.kind === 'taken') return line(t('approve.taken'))
  if (local.kind === 'gone' || !item) return line(t('approve.gone'))

  // ---- 待处理 ----
  const hint = displayHint(item.info.hint)
  const left = item.expiresAt - now
  if (left <= 0) return line(t('approve.expired'))
  const isScan = item.info.method === 'scan'
  const busy = local.kind === 'busy'
  return (
    <div className="space-y-3" data-testid="approve-pending" data-method={item.info.method ?? ''}>
      <div>
        <p className="font-medium" data-testid="approve-hint-name">
          {hint.name ?? t('approve.unknownDevice')}
        </p>
        <p className="text-xs text-fg-subtle" data-testid="approve-hint-meta">
          {hint.platform ? `${hint.platform} · ` : ''}
          {t('approve.unverified')} · {formatRelative(item.createdAt, i18n.language, now)} · {t('approve.timeLeft', { time: formatCountdown(left) })}
        </p>
      </div>
      <p className="text-sm font-medium">{t('approve.onlyThisOne')}</p>
      <p className="text-sm text-fg-muted">{isScan ? t('approve.scanOnly') : t('approve.ifNotYou')}</p>
      <div className="flex justify-end gap-2">
        <Button variant="outline" disabled={busy} loading={local.kind === 'busy' && local.action === 'deny'} onClick={() => void run('deny')} data-testid="approve-deny">
          {t('approve.deny')}
        </Button>
        {isScan ? (
          <Button variant="primary" disabled={busy || !onScan} onClick={onScan} data-testid="approve-scan">
            {t('approve.scan')}
          </Button>
        ) : (
          <Button variant="primary" disabled={busy} loading={local.kind === 'busy' && local.action === 'approve'} onClick={() => void run('approve')} data-testid="approve-approve">
            {t('approve.approve')}
          </Button>
        )}
      </div>
    </div>
  )
}
