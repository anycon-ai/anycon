import { useEffect } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { ChevronRight, Lock } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useDevices } from '@/api/queries'
import { CenterLayout } from '@/components/CenterLayout'
import { accountNodes, lockedNodes, unlockParam } from '@/features/unlock/nodes'
import { cleanHintText } from '@/lib/hint'
import { formatRelative } from '@/lib/format'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Card, Dot, Spinner } from '@/ui/Misc'

/**
 * `/unlock`（ADR-36，spec/web.md § 4.3.1）：账号下、这个浏览器还没配对的节点。只有一台就直接进它的解锁页。
 * 名称和平台来自 hub、没有经过认证，只用来挑节点；解锁完成后以 `pair.ack` 里的名字为准。
 */
export function UnlockPage() {
  const { t, i18n } = useTranslation()
  const navigate = useNavigate()
  const devices = useDevices({ refetchMs: 8000 })
  const order = useNodes((s) => s.order)
  const loaded = useNodes((s) => s.loaded)

  const all = accountNodes(devices.data)
  const locked = lockedNodes(devices.data, order)
  const single = locked.length === 1 ? locked[0]! : null

  useEffect(() => {
    if (!devices.data || !loaded) return
    if (single) void navigate({ to: '/unlock/$node', params: { node: unlockParam(single, all) }, replace: true })
    // 账号下一台节点都没有 → 向导；都已经配对了 → 回首页
    else if (all.length === 0) void navigate({ to: '/setup', replace: true })
    else if (locked.length === 0) void navigate({ to: '/', replace: true })
  }, [devices.data, loaded, single, all, locked.length, navigate])

  if (devices.isError)
    return (
      <CenterLayout medium>
        <Card className="space-y-3 p-5 text-center" testId="unlock-load-failed">
          <p className="text-sm text-danger">{t('unlock.loadFailed')}</p>
          <Button onClick={() => void devices.refetch()}>{t('app.retry')}</Button>
        </Card>
      </CenterLayout>
    )
  if (devices.isPending || !loaded || locked.length <= 1)
    return (
      <div className="flex min-h-dvh items-center justify-center" aria-busy="true">
        <Spinner className="size-6" />
      </div>
    )

  return (
    <CenterLayout medium>
      <Card className="p-5" testId="unlock-list">
        <h1 className="text-lg font-semibold">{t('unlock.list.title')}</h1>
        <p className="mt-1 mb-4 text-sm text-fg-muted">{t('unlock.list.subtitle')}</p>
        <ul className="divide-y divide-border overflow-hidden rounded-lg border border-border">
          {locked.map((n) => (
            <li key={n.id}>
              <button
                type="button"
                className="flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-bg-hover"
                onClick={() => void navigate({ to: '/unlock/$node', params: { node: unlockParam(n, all) } })}
                data-testid="unlock-node-row"
                data-node={n.id}
              >
                <Lock className="size-4 shrink-0 text-fg-subtle" aria-hidden />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-medium">{cleanHintText(n.name) || n.id.slice(0, 8)}</span>
                  <span className="flex items-center gap-1.5 text-xs text-fg-subtle">
                    <Dot tone={n.online ? 'success' : 'neutral'} />
                    {n.online ? t('devices.online') : n.last_seen_ms ? t('devices.lastSeen', { when: formatRelative(n.last_seen_ms, i18n.language) }) : t('devices.offline')}
                    <span>·</span>
                    <span className="truncate">{cleanHintText(n.platform)}</span>
                  </span>
                </span>
                <ChevronRight className="size-4 shrink-0 text-fg-subtle" aria-hidden />
              </button>
            </li>
          ))}
        </ul>
      </Card>
      <div className="mt-4 text-center">
        <Button variant="ghost" onClick={() => void navigate({ to: '/pair' })} data-testid="unlock-use-code">
          {t('unlock.usePairCode')}
        </Button>
      </div>
    </CenterLayout>
  )
}
