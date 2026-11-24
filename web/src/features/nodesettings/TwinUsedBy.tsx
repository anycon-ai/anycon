// 「Bot」标签里的「被别的节点使用」：这个节点上配过对的 Bot 设备（别的节点的 Bot），可以逐个设定它能在这里做什么。
// 撤销 Bot 设备仍在「设备」标签里做，这里不重复。
import { useCallback, useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { NodeDevicesResult, PeerDevice } from '@/protocol'
import { shortNodeId } from '@/lib/ids'
import { rpc } from '@/transport/instance'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Card, Chip, Dot, SectionTitle, Skeleton } from '@/ui/Misc'
import { TwinLinkScope } from './TwinLinkScope'

export function TwinUsedBy({ node }: { node: NodeEntry }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const [loading, setLoading] = useState(false)
  const [scope, setScope] = useState<PeerDevice | null>(null)
  const offline = node.conn !== 'ready'
  const twins = node.devices.filter((d) => d.kind === 'twin')
  const nodeId = node.id
  const load = useCallback(async () => {
    setLoading(true)
    try {
      const res = await rpc().request<NodeDevicesResult>(nodeId, 'node.devices', {})
      useNodes.getState().setDevices(nodeId, res.devices)
    } catch (e) {
      console.warn('anycon: could not load the node device list; showing the cached one', e)
    } finally {
      setLoading(false)
    }
  }, [nodeId])
  useEffect(() => {
    if (!offline) void load()
  }, [offline, load])

  return (
    <section data-testid="twin-tab-used-by">
      <SectionTitle>{t('nodeSettings.twin.usedByTitle')}</SectionTitle>
      <div className="space-y-3">
        <p className="text-sm text-fg-muted">{t('nodeSettings.twin.usedByHint')}</p>
        {node.info?.twin_accept_links === false ? (
          <p className="text-sm text-warning" data-testid="twin-tab-accept-off">
            {t('nodeSettings.twin.acceptOff')}
          </p>
        ) : null}
        {loading && !twins.length ? <Skeleton className="h-12" /> : null}
        {!loading && !twins.length ? <p className="text-sm text-fg-subtle">{t('nodeSettings.twin.usedByNone')}</p> : null}
        <ul className="space-y-2" data-testid="twin-tab-devices">
          {twins.map((d) => (
            <li key={d.device_id}>
              <Card className="flex items-center gap-3 p-3">
                <div className="min-w-0 flex-1">
                  <p className="flex flex-wrap items-center gap-2 font-medium">
                    <span className="truncate">{d.name}</span>
                    <Chip>{t('twinScope.kind')}</Chip>
                  </p>
                  <p className="inline-flex items-center gap-1 text-xs text-fg-subtle">
                    <Dot tone={d.online ? 'success' : 'neutral'} />
                    {d.online ? t('devices.online') : t('devices.offline')}
                  </p>
                </div>
                <Button variant="outline" size="lg" disabled={offline} onClick={() => setScope(d)} data-testid="twin-tab-device-scope">
                  {t('twinScope.button')}
                </Button>
              </Card>
            </li>
          ))}
        </ul>
        <p className="text-sm text-fg-subtle">
          {t('nodeSettings.twin.revokeHint')}{' '}
          <Link to="/n/$node/settings" params={{ node: shortNodeId(nodeId, order) }} search={{ tab: 'devices' }} className="text-accent-text" data-testid="twin-tab-devices-link">
            {t('nodeSettings.twin.openDevices')}
          </Link>
        </p>
      </div>
      <TwinLinkScope nodeId={nodeId} device={scope} onClose={() => setScope(null)} />
    </section>
  )
}
