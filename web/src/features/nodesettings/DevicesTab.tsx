import { useCallback, useEffect, useRef, useState } from 'react'
import { Laptop, Plus, RefreshCw, ScanLine, Server, Smartphone, Trash2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { NodeDevicesResult, NodeEnrollCodeResult, NodePairBeginParams, NodePairBeginResult, PeerDevice } from '@/protocol'
import { groupCode } from '@/features/approve/actions'
import { useCameraAvailable } from '@/features/approve/useCameraAvailable'
import { revokeDevice } from '@/features/devices/revoke'
import { localExpiry } from '@/lib/expiry'
import { formatRelative } from '@/lib/format'
import { describeError } from '@/lib/errors'
import { useNow } from '@/lib/useNow'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Card, Chip, CopyButton, Dot, Segmented, Skeleton, Spinner, Switch } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'
import { QrCode } from '@/ui/QrCode'

export function DevicesTab({ nodeId }: { nodeId: string }) {
  const { t, i18n } = useTranslation()
  const devices = useNodes((s) => s.byId[nodeId]?.devices)
  const offline = useNodes((s) => s.byId[nodeId]?.conn !== 'ready')
  const self = useHub((s) => s.snapshot.deviceId)
  const [loading, setLoading] = useState(false)
  const [revoke, setRevoke] = useState<PeerDevice | null>(null)
  const [pairing, setPairing] = useState<NodePairBeginResult | null>(null)
  // 「添加设备」对话框：短码（配对）和接入码（ADR-36）两页
  const [addOpen, setAddOpen] = useState(false)
  const [page, setPage] = useState<'pair' | 'code'>('pair')
  // 接入码页第一次切到才生成（新的顶掉旧的）；之后来回切换不重新生成，关掉对话框才丢
  const [codeShown, setCodeShown] = useState(false)
  const camera = useCameraAvailable()
  // 16 位长码默认开：二维码 / 链接不用手输，长码也让经不信任的 hub 配对更难被穷举（ADR-29）；要手输时可以关掉换成 8 位
  const [long, setLong] = useState(true)
  const [busy, setBusy] = useState(false)

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

  const addDevice = async (longCode: boolean): Promise<void> => {
    setBusy(true)
    try {
      const params: NodePairBeginParams = { long: longCode }
      setPairing(await rpc().request<NodePairBeginResult>(nodeId, 'node.pair_begin', params))
    } catch (e) {
      setAddOpen(false)
      toast.error(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }

  const doRevoke = async (d: PeerDevice): Promise<void> => {
    setBusy(true)
    try {
      // 两步：daemon 删公钥，hub 删路由 / 配对（spec/hub.md § 2.3）。任何一步失败都要让用户看到，对话框留着可以重试
      await revokeDevice(d.device_id, [nodeId])
      setRevoke(null)
      await load()
    } catch (e) {
      toast.error(t('devices.revokeFailed', { error: describeError(e, t) }))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3">
        <p className="text-sm text-fg-muted">{t('nodeSettings.devicesHint')}</p>
        <div className="flex shrink-0 flex-wrap justify-end gap-2">
          {camera ? (
            <Button variant="outline" size="sm" icon={<ScanLine className="size-4" />} disabled={offline} onClick={() => useEnroll.getState().openScanner()} data-testid="device-scan-approve">
              {t('nodeSettings.scanApprove')}
            </Button>
          ) : null}
          <Button
            variant="primary"
            size="sm"
            icon={<Plus className="size-4" />}
            loading={busy && !revoke}
            disabled={offline}
            onClick={() => {
              setPage('pair')
              setCodeShown(false)
              setPairing(null)
              setAddOpen(true)
              void addDevice(long)
            }}
            data-testid="device-add"
          >
            {t('nodeSettings.addDevice')}
          </Button>
        </div>
      </div>
      {loading && !devices?.length ? <Skeleton className="h-16" /> : null}
      <ul className="space-y-2" data-testid="device-list">
        {(devices ?? []).map((d) => (
          <li key={d.device_id}>
            <Card className="flex items-center gap-3 p-3">
              <span className="text-fg-subtle">{d.kind === 'web' ? <Laptop className="size-5" /> : d.kind === 'daemon' ? <Server className="size-5" /> : <Smartphone className="size-5" />}</span>
              <div className="min-w-0 flex-1" data-device={d.device_id}>
                <p className="flex flex-wrap items-center gap-2 font-medium">
                  <span className="truncate">{d.name}</span>
                  {d.device_id === self ? <Chip tone="accent">{t('devices.thisBrowser')}</Chip> : null}
                  <Chip>{d.kind}</Chip>
                </p>
                <p className="flex flex-wrap items-center gap-x-3 text-xs text-fg-subtle">
                  <span className="inline-flex items-center gap-1">
                    <Dot tone={d.online ? 'success' : 'neutral'} />
                    {d.online ? t('devices.online') : d.last_seen_ms ? t('devices.lastSeen', { when: formatRelative(d.last_seen_ms, i18n.language) }) : t('devices.offline')}
                  </span>
                  <span className="font-mono">{d.fingerprint}</span>
                </p>
              </div>
              <Button variant="outline-danger" size="sm" icon={<Trash2 className="size-4" />} disabled={offline} onClick={() => setRevoke(d)} aria-label={t('devices.revoke')}>
                <span className="hidden sm:inline">{t('devices.revoke')}</span>
              </Button>
            </Card>
          </li>
        ))}
      </ul>

      <Dialog open={addOpen} onOpenChange={(o) => !o && (setAddOpen(false), setPairing(null))} title={t('nodeSettings.addDeviceTitle')} description={page === 'pair' ? t('nodeSettings.addDeviceBody') : t('nodeSettings.enrollCodeBody')} closeLabel={t('common.close')} testId="pair-begin-dialog">
        <div className="flex flex-col items-center gap-4">
          <div className="w-full">
            <Segmented
              ariaLabel={t('nodeSettings.addDeviceTitle')}
              value={page}
              onChange={(v) => {
                setPage(v)
                if (v === 'code') setCodeShown(true)
              }}
              fill
              options={[
                { value: 'pair', label: t('nodeSettings.pageShortCode'), testId: 'add-page-pair' },
                { value: 'code', label: t('nodeSettings.pageAccessCode'), testId: 'add-page-code' },
              ]}
            />
          </div>
          {page === 'pair' ? (
            pairing ? (
              <>
                <p className={cn('font-mono font-semibold', pairing.code.length > 9 ? 'text-lg tracking-[0.1em]' : 'text-3xl tracking-[0.2em]')} data-testid="pair-begin-code">
                  {pairing.code}
                </p>
                <label className="flex items-center gap-2 text-sm text-fg-muted" htmlFor="pair-long">
                  <Switch id="pair-long" checked={long} disabled={busy} label={t('nodeSettings.longCode')} onCheckedChange={(v) => { setLong(v); void addDevice(v) }} />
                  <span>
                    {t('nodeSettings.longCode')}
                    <span className="block text-xs text-fg-subtle">{t('nodeSettings.longCodeHint')}</span>
                  </span>
                </label>
                <QrCode text={pairing.link} label={t('nodeSettings.qr')} />
                <div className="flex items-center gap-2 text-xs text-fg-subtle">
                  <span className="max-w-60 truncate font-mono">{pairing.link}</span>
                  <CopyButton text={pairing.link} />
                </div>
                <Countdown until={pairing.expires_at_ms} />
              </>
            ) : (
              <Spinner className="size-5" />
            )
          ) : null}
          {codeShown ? (
            <div className={cn('w-full', page !== 'code' && 'hidden')}>
              <AccessCodePage nodeId={nodeId} />
            </div>
          ) : null}
        </div>
      </Dialog>

      <Dialog open={!!revoke} onOpenChange={(o) => !o && setRevoke(null)} title={t('devices.revokeTitle', { name: revoke?.name ?? '' })} description={t('devices.revokeBody')} closeLabel={t('common.close')}>
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setRevoke(null)}>
            {t('common.cancel')}
          </Button>
          <Button variant="danger" loading={busy} onClick={() => revoke && void doRevoke(revoke)} data-testid="device-revoke-confirm">
            {t('devices.revoke')}
          </Button>
        </div>
      </Dialog>
    </div>
  )
}

function Countdown({ until }: { until: number }) {
  const { t } = useTranslation()
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [])
  const left = Math.max(0, Math.round((until - now) / 1000))
  return <p className="text-xs text-fg-subtle">{left > 0 ? t('nodeSettings.expiresIn', { time: `${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}` }) : t('nodeSettings.expired')}</p>
}

/**
 * 「添加设备」的接入码页（ADR-36）：`node.enroll_code` 生成一个 6 位码，5 分钟内本账号任一设备用「我有接入码」都可以用。
 * 切到这一页才生成（新的会顶掉旧的）；关掉对话框不作废，过期后可以重新生成。
 */
function AccessCodePage({ nodeId }: { nodeId: string }) {
  const { t } = useTranslation()
  const [code, setCode] = useState<{ code: string; expiresAt: number } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const now = useNow(1000, !!code)

  const generate = useCallback(async () => {
    setBusy(true)
    setError(null)
    try {
      const res = await rpc().request<NodeEnrollCodeResult>(nodeId, 'node.enroll_code', {})
      setCode({ code: res.code, expiresAt: localExpiry(res.expires_at_ms, Date.now()) })
    } catch (e) {
      // 批准接入关着时 daemon 不生成码（forbidden）
      setError(e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'forbidden' ? t('nodeSettings.enrollCodeDisabled') : describeError(e, t))
    } finally {
      setBusy(false)
    }
  }, [nodeId, t])

  const started = useRef(false)
  useEffect(() => {
    if (started.current) return
    started.current = true
    void generate()
  }, [generate])

  const left = code ? code.expiresAt - now : 0
  return (
    <div className="flex w-full flex-col items-center gap-4" data-testid="enroll-code-page">
      {error ? (
        <p className="text-sm text-danger" role="alert" data-testid="enroll-code-error">
          {error}
        </p>
      ) : code && left > 0 ? (
        <>
          <p className="font-mono text-5xl font-semibold tracking-[0.2em]" data-testid="enroll-code" aria-label={code.code.split('').join(' ')}>
            {groupCode(code.code)}
          </p>
          <p className="text-center text-sm text-fg-muted">{t('nodeSettings.enrollCodeHow')}</p>
          <p className="text-xs text-fg-subtle" data-testid="enroll-code-countdown">
            {t('nodeSettings.expiresIn', { time: `${Math.floor(left / 60000)}:${String(Math.floor((left % 60000) / 1000)).padStart(2, '0')}` })}
          </p>
          <div className="flex items-center gap-2">
            <CopyButton text={code.code} />
          </div>
        </>
      ) : code ? (
        <p className="text-sm text-fg-muted" data-testid="enroll-code-expired">
          {t('nodeSettings.expired')}
        </p>
      ) : (
        <Spinner className="size-5" />
      )}
      <Button variant="outline" size="sm" icon={<RefreshCw className="size-4" />} loading={busy} onClick={() => void generate()} data-testid="enroll-code-regenerate">
        {t('nodeSettings.regenerate')}
      </Button>
    </div>
  )
}
