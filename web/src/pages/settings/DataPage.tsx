import { useCallback, useEffect, useState } from 'react'
import { Progress } from 'radix-ui'
import { useTranslation } from 'react-i18next'
import { KEEP_SESSIONS, MAX_EVENTS_PER_SESSION } from '@/features/storage/eventCache'
import { clearLocalCaches, readCacheStats, readPersist, readUsage, requestPersist, type CacheStats, type PersistState, type StorageUsage } from '@/features/storage/localData'
import { describeError } from '@/lib/errors'
import { formatBytes } from '@/lib/format'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Card, Chip, SectionTitle, Skeleton } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'
import { cn } from '@/ui/cn'

interface Snapshot {
  usage: StorageUsage | null
  persist: PersistState
  /** 读不出来（库被禁用、浏览器拒绝访问）时是 null。 */
  cache: CacheStats | null
}

async function readSnapshot(): Promise<Snapshot> {
  const [usage, persist, cache] = await Promise.all([
    readUsage(),
    readPersist(),
    readCacheStats().catch((e: unknown) => {
      console.warn('anycon: could not read the cache size', e)
      return null
    }),
  ])
  return { usage, persist, cache }
}

/**
 * `/settings/data`：这个浏览器替 anycon 存了多少、浏览器允许多少，清空会话历史和附件的缓存，
 * 以及请求持久化存储（空间紧张时浏览器不删这个网站的数据）。「没能在本地保存数据」的横幅指到这里。
 */
export function DataPage() {
  const { t } = useTranslation()
  const [snap, setSnap] = useState<Snapshot | null>(null)
  const [confirming, setConfirming] = useState(false)
  const [clearing, setClearing] = useState(false)
  const [asking, setAsking] = useState(false)
  const [denied, setDenied] = useState(false)

  const reload = useCallback(async () => setSnap(await readSnapshot()), [])
  useEffect(() => {
    void reload()
  }, [reload])

  const doClear = async () => {
    setClearing(true)
    try {
      await clearLocalCaches()
      toast.success(t('localData.cleared'))
      setConfirming(false)
    } catch (e) {
      toast.error(t('localData.clearFailed', { error: describeError(e, t) }))
    } finally {
      setClearing(false)
      await reload()
    }
  }

  const doPersist = async () => {
    setAsking(true)
    try {
      const state = await requestPersist()
      setDenied(state !== 'persisted')
      setSnap((s) => (s ? { ...s, persist: state } : s))
    } finally {
      setAsking(false)
    }
  }

  return (
    <div className="space-y-6">
      <div className="space-y-1">
        <h1 className="text-xl font-semibold">{t('settings.data')}</h1>
        <p className="text-sm text-fg-muted">{t('localData.hint')}</p>
      </div>

      {!snap ? (
        <Skeleton className="h-48" />
      ) : (
        <>
          <section className="space-y-2">
            <SectionTitle>{t('localData.usageTitle')}</SectionTitle>
            <Card className="space-y-2 px-4 py-3" testId="data-usage">
              {snap.usage ? <UsageBar usage={snap.usage} /> : <p className="text-sm text-fg-muted">{t('localData.usageUnknown')}</p>}
              <p className="text-xs text-fg-subtle">{t('localData.usageHint')}</p>
            </Card>
          </section>

          <section className="space-y-2">
            <SectionTitle>{t('localData.cacheTitle')}</SectionTitle>
            <Card className="divide-y divide-border" testId="data-cache">
              <div className="space-y-1 px-4 py-3">
                <div className="flex items-baseline justify-between gap-3">
                  <p className="text-sm font-medium">{t('localData.history')}</p>
                  <p className="shrink-0 font-mono text-sm" data-testid="data-cache-events">
                    {snap.cache ? t('localData.events', { count: snap.cache.events }) : '—'}
                  </p>
                </div>
                <p className="text-xs text-fg-subtle" data-testid="data-cache-history-hint">
                  {t('localData.historyHint', { sessions: KEEP_SESSIONS, events: MAX_EVENTS_PER_SESSION })}
                </p>
              </div>
              <div className="flex items-baseline justify-between gap-3 px-4 py-3">
                <p className="text-sm font-medium">{t('localData.attachments')}</p>
                <p className="shrink-0 font-mono text-sm" data-testid="data-cache-attachments">
                  {snap.cache ? t('localData.files', { count: snap.cache.attachments.count, size: formatBytes(snap.cache.attachments.bytes) }) : '—'}
                </p>
              </div>
              <div className="space-y-2 px-4 py-3">
                <p className="text-xs text-fg-subtle">{t('localData.clearHint')}</p>
                <Button variant="outline" onClick={() => setConfirming(true)} data-testid="data-clear">
                  {t('localData.clear')}
                </Button>
              </div>
            </Card>
          </section>

          <section className="space-y-2">
            <SectionTitle>{t('localData.persistTitle')}</SectionTitle>
            <Card className="space-y-2 px-4 py-3" testId="data-persist">
              <p className="flex items-center gap-2 text-sm font-medium" data-testid="data-persist-state" data-state={snap.persist}>
                {snap.persist === 'persisted' ? <Chip tone="success">{t('localData.persistOnChip')}</Chip> : snap.persist === 'best_effort' ? <Chip tone="warning">{t('localData.persistOffChip')}</Chip> : null}
                <span>{t(`localData.persist.${snap.persist}`)}</span>
              </p>
              {snap.persist === 'best_effort' ? (
                <Button variant="outline" loading={asking} onClick={() => void doPersist()} data-testid="data-persist-ask">
                  {t('localData.persistAsk')}
                </Button>
              ) : null}
              {denied && snap.persist === 'best_effort' ? (
                <p role="status" className="text-xs text-fg-muted" data-testid="data-persist-denied">
                  {t('localData.persistDenied')}
                </p>
              ) : null}
            </Card>
          </section>
        </>
      )}

      <Dialog open={confirming} onOpenChange={(o) => !clearing && setConfirming(o)} title={t('localData.clearTitle')} description={t('localData.clearBody')} closeLabel={t('common.close')}>
        <div className="flex justify-end gap-2">
          <Button variant="outline" disabled={clearing} onClick={() => setConfirming(false)}>
            {t('common.cancel')}
          </Button>
          <Button variant="danger" loading={clearing} onClick={() => void doClear()} data-testid="data-clear-confirm">
            {t('localData.clear')}
          </Button>
        </div>
      </Dialog>
    </div>
  )
}

function UsageBar({ usage }: { usage: StorageUsage }) {
  const { t } = useTranslation()
  const ratio = Math.min(1, usage.usage / usage.quota)
  const near = ratio >= 0.8
  return (
    <>
      <div className="flex items-baseline justify-between gap-3">
        <p className="text-sm font-medium">{t('localData.usageLabel')}</p>
        <p className="shrink-0 font-mono text-sm" data-testid="data-usage-value">
          {t('localData.usage', { used: formatBytes(usage.usage), quota: formatBytes(usage.quota) })}
        </p>
      </div>
      <Progress.Root value={Math.min(usage.usage, usage.quota)} max={usage.quota} aria-label={t('localData.usageLabel')} className="h-1.5 overflow-hidden rounded-full bg-bg-muted">
        <Progress.Indicator className={cn('h-full rounded-full transition-[width]', near ? 'bg-warning' : 'bg-accent')} style={{ width: `${ratio * 100}%` }} />
      </Progress.Root>
    </>
  )
}
