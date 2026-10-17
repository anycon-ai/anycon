import { useMemo, useState } from 'react'
import { Laptop, Server, Terminal } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { ApiDevice } from '@/api/types'
import { useDevices, useRenameDevice, useRevokeDevice } from '@/api/queries'
import { describeError } from '@/lib/errors'
import { formatRelative } from '@/lib/format'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Input } from '@/ui/Field'
import { Card, Chip, Dot, SectionTitle, Skeleton } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

const KIND_ICON = { daemon: Server, web: Laptop, cli: Terminal, sdk: Terminal } as const

export function DevicesPage() {
  const { t, i18n } = useTranslation()
  const devices = useDevices()
  const revoke = useRevokeDevice()
  const rename = useRenameDevice()
  const self = useHub((s) => s.snapshot.deviceId)
  const [target, setTarget] = useState<ApiDevice | null>(null)
  const [editing, setEditing] = useState<ApiDevice | null>(null)
  const [name, setName] = useState('')

  // 按节点（daemon）分组，显示配对关系
  const groups = useMemo(() => {
    const all = devices.data ?? []
    const daemons = all.filter((d) => d.kind === 'daemon')
    const clients = all.filter((d) => d.kind !== 'daemon')
    return daemons.map((n) => ({ node: n, clients: clients.filter((c) => c.paired_with.includes(n.id)) }))
  }, [devices.data])
  const unpaired = (devices.data ?? []).filter((d) => d.kind !== 'daemon' && !(devices.data ?? []).some((n) => n.kind === 'daemon' && d.paired_with.includes(n.id)))

  const doRevoke = (d: ApiDevice): void => {
    // 先让能连上的 daemon 删公钥，再让 hub 删路由（spec/hub.md § 2.3：UI 两步一起做）。daemon 本身被吊销时只有 hub 这一步。
    revoke.mutate(
      { id: d.id, nodeIds: d.kind === 'daemon' ? [] : d.paired_with },
      {
        onSuccess: ({ offlineNodes }) => {
          setTarget(null)
          if (d.kind === 'daemon') useNodes.getState().patch(d.id, { conn: 'noise_failed' })
          if (offlineNodes.length > 0) toast.info(t('devices.revokedOffline', { count: offlineNodes.length }))
        },
        // 失败要说出来，对话框留着可以重试
        onError: (e) => toast.error(t('devices.revokeFailed', { error: describeError(e, t) })),
      },
    )
  }

  const row = (d: ApiDevice, indent = false) => {
    const Icon = KIND_ICON[d.kind]
    return (
      <Card key={d.id} className={`flex items-center gap-3 p-3 ${indent ? 'ml-6' : ''}`} testId="device-row">
        <Icon className="size-5 shrink-0 text-fg-subtle" aria-hidden />
        <div className="min-w-0 flex-1">
          <p className="flex flex-wrap items-center gap-2 text-sm font-medium">
            <span className="truncate">{d.name}</span>
            {d.id === self ? <Chip tone="accent">{t('devices.thisBrowser')}</Chip> : null}
            <Chip>{d.kind}</Chip>
          </p>
          <p className="flex flex-wrap items-center gap-x-3 text-xs text-fg-subtle">
            <span className="inline-flex items-center gap-1">
              <Dot tone={d.online ? 'success' : 'neutral'} />
              {d.online ? t('devices.online') : d.last_seen_ms ? t('devices.lastSeen', { when: formatRelative(d.last_seen_ms, i18n.language) }) : t('devices.offline')}
            </span>
            <span>{d.platform}</span>
            {d.origin ? <span className="truncate">{d.origin}</span> : null}
          </p>
        </div>
        <Button size="sm" variant="ghost" onClick={() => { setEditing(d); setName(d.name) }}>
          {t('devices.rename')}
        </Button>
        <Button size="sm" variant="outline-danger" onClick={() => setTarget(d)} data-testid="device-revoke">
          {t('devices.revoke')}
        </Button>
      </Card>
    )
  }

  return (
    <div className="space-y-6">
      <h1 className="text-xl font-semibold">{t('settings.devices')}</h1>
      <p className="text-sm text-fg-muted">{t('settings.devicesHint')}</p>
      {devices.isLoading ? <Skeleton className="h-20" /> : null}
      {groups.map(({ node, clients }) => (
        <section key={node.id} className="space-y-2">
          <SectionTitle>{node.name}</SectionTitle>
          {row(node)}
          {clients.map((c) => row(c, true))}
        </section>
      ))}
      {unpaired.length > 0 ? (
        <section className="space-y-2">
          <SectionTitle>{t('settings.unpaired')}</SectionTitle>
          {unpaired.map((d) => row(d))}
        </section>
      ) : null}

      <Dialog open={!!target} onOpenChange={(o) => !o && setTarget(null)} title={t('devices.revokeTitle', { name: target?.name ?? '' })} description={target?.id === self ? t('devices.revokeSelf') : t('devices.revokeBody')} closeLabel={t('common.close')}>
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setTarget(null)}>
            {t('common.cancel')}
          </Button>
          <Button variant="danger" loading={revoke.isPending} onClick={() => target && doRevoke(target)} data-testid="device-revoke-confirm">
            {t('devices.revoke')}
          </Button>
        </div>
      </Dialog>
      <Dialog open={!!editing} onOpenChange={(o) => !o && setEditing(null)} title={t('devices.rename')} closeLabel={t('common.close')}>
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault()
            if (!editing || !name.trim()) return
            rename.mutate({ id: editing.id, name: name.trim() }, { onSuccess: () => setEditing(null), onError: (err) => toast.error(describeError(err, t)) })
          }}
        >
          <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus maxLength={80} aria-label={t('devices.rename')} />
          <div className="flex justify-end">
            <Button type="submit" variant="primary" loading={rename.isPending} disabled={!name.trim()}>
              {t('common.save')}
            </Button>
          </div>
        </form>
      </Dialog>
    </div>
  )
}

