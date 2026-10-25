import { useEffect, useRef } from 'react'
import { Link } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Progress } from 'radix-ui'
import { useTranslation } from 'react-i18next'
import { accountKeys, useQuota } from '@/api/accounts'
import type { ApiQuota } from '@/api/types'
import { describeError } from '@/lib/errors'
import { useHub } from '@/stores/hub'
import { Card, Chip, Skeleton } from '@/ui/Misc'
import { cn } from '@/ui/cn'

type Row = 'devices' | 'daemons' | 'connections' | 'dailyPairings'

function rowValues(q: ApiQuota, row: Row): { used: number; max: number | null } {
  switch (row) {
    case 'devices':
      return { used: q.devices, max: q.devices_max }
    case 'daemons':
      return { used: q.daemons, max: q.daemons_max }
    case 'connections':
      return { used: q.connections, max: q.connections_max }
    case 'dailyPairings':
      return { used: q.daily_pairings, max: q.daily_pairings_max }
  }
}

const ROWS: Row[] = ['devices', 'daemons', 'connections', 'dailyPairings']

/**
 * `/settings/quota`（只有 `multi`）：设备、节点、同时在线的连接、滚动 24 小时内的配对次数各用了多少、上限多少。
 * hub 在这些数变化时通过 `quota_update` 推给在线的连接（存在 hub store 里）：它一变，这里就重新拉一次 `/api/quota`
 * （推送里没有「今日配对次数」，所以不直接用推送的数）。
 */
export function QuotaPage() {
  const { t } = useTranslation()
  const qc = useQueryClient()
  const quota = useQuota()
  const pushed = useHub((s) => s.quota)
  const seen = useRef(pushed)
  useEffect(() => {
    if (seen.current === pushed) return
    seen.current = pushed
    void qc.invalidateQueries({ queryKey: accountKeys.quota })
  }, [pushed, qc])

  return (
    <div className="space-y-4">
      <h1 className="text-xl font-semibold">{t('settings.quota')}</h1>
      <p className="text-sm text-fg-muted">
        {t('quota.hint')}{' '}
        <Link to="/settings/devices" className="text-accent-text hover:underline" data-testid="quota-devices-link">
          {t('quota.manageDevices')}
        </Link>
      </p>
      {quota.isLoading ? <Skeleton className="h-48" /> : null}
      {quota.isError ? (
        <p role="alert" className="text-sm text-danger">
          {describeError(quota.error, t)}
        </p>
      ) : null}
      {quota.data ? (
        <Card className="divide-y divide-border" testId="quota-list">
          {ROWS.map((row) => (
            <QuotaRow key={row} row={row} {...rowValues(quota.data, row)} />
          ))}
        </Card>
      ) : null}
    </div>
  )
}

function QuotaRow({ row, used, max }: { row: Row; used: number; max: number | null }) {
  const { t } = useTranslation()
  const limited = max !== null
  const full = limited && used >= max
  const near = limited && !full && max > 0 && used / max >= 0.8
  const label = t(`quota.rows.${row}.label`)
  return (
    <div className="space-y-2 px-4 py-3" data-testid={`quota-${row}`} data-used={used} data-max={max ?? ''}>
      <div className="flex items-baseline justify-between gap-3">
        <p className="flex min-w-0 items-center gap-2 text-sm font-medium">
          <span className="truncate">{label}</span>
          {full ? <Chip tone="danger">{t('quota.full')}</Chip> : near ? <Chip tone="warning">{t('quota.nearLimit')}</Chip> : null}
        </p>
        <p className="shrink-0 font-mono text-sm" data-testid={`quota-${row}-value`}>
          {limited ? t('quota.usedOfMax', { used, max }) : t('quota.usedUnlimited', { used })}
        </p>
      </div>
      {limited ? (
        <Progress.Root value={Math.min(used, max)} max={Math.max(max, 1)} aria-label={label} className="h-1.5 overflow-hidden rounded-full bg-bg-muted">
          <Progress.Indicator className={cn('h-full rounded-full transition-[width]', full ? 'bg-danger' : near ? 'bg-warning' : 'bg-accent')} style={{ width: `${Math.min(100, (used / Math.max(max, 1)) * 100)}%` }} />
        </Progress.Root>
      ) : null}
      <p className="text-xs text-fg-subtle">{t(`quota.rows.${row}.hint`)}</p>
    </div>
  )
}
