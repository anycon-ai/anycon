import { useEffect, useRef, useState } from 'react'
import { Check, CameraOff, ScanLine, ShieldAlert } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { EnrollInfo } from '@/protocol'
import { describeError } from '@/lib/errors'
import { displayHint } from '@/lib/hint'
import { parseEnrollQr } from '@/lib/enrollQr'
import { hexOf } from '@/transport/wire'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'
import { Spinner } from '@/ui/Misc'
import { approveScan, isAlreadyResolved, isGone, refreshEnrolls } from './actions'
import { CameraError, createQrReader, openCamera } from './scanner'

type View =
  | { kind: 'starting' }
  | { kind: 'scanning' }
  | { kind: 'denied' }
  | { kind: 'unavailable' }
  | { kind: 'loading' }
  | { kind: 'confirm'; nodeId: string; enrollId: string; info: EnrollInfo }
  | { kind: 'missing' }
  /** 二维码对应的请求不是 `scan`（比如「请求批准」的会话）：不能用这把密钥批准。 */
  | { kind: 'notScan' }
  /** 是 `scan` 请求，但不在等批准（还没收到 hello，或已经批准过了）。 */
  | { kind: 'notWaiting' }
  | { kind: 'approving'; nodeId: string; enrollId: string; info: EnrollInfo }
  | { kind: 'done' }
  | { kind: 'failed'; message: string }

type Notice = { kind: 'notAnycon' } | { kind: 'unpaired'; node: string }

const SCAN_INTERVAL_MS = 200

/**
 * 应用内扫码批准（ADR-36，spec/web.md § 4.3.2）：新设备选了「用已配对的设备扫码」，这里扫它屏幕上的二维码。
 * 只认 `anycon-enroll:1:` 开头的内容、而且节点必须是这个浏览器配对过的；认出来立刻停摄像头，确认后才批准。
 * 扫到的密钥只在内存里（组件的 ref），关掉对话框就清零丢弃。
 */
export function ScanApproveDialog() {
  const { t } = useTranslation()
  const open = useEnroll((s) => s.scannerOpen)
  const close = useEnroll((s) => s.closeScanner)
  return (
    <Dialog open={open} onOpenChange={(o) => !o && close()} title={t('scan.title')} closeLabel={t('common.close')} testId="scan-dialog">
      {open ? <ScanSession onClose={close} /> : null}
    </Dialog>
  )
}

function ScanSession({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation()
  const videoRef = useRef<HTMLVideoElement>(null)
  const secretRef = useRef<Uint8Array | null>(null)
  const [view, setView] = useState<View>({ kind: 'starting' })
  const [notice, setNotice] = useState<Notice | null>(null)
  const [attempt, setAttempt] = useState(0)
  const nodes = useNodes((s) => s.byId)

  const wipeSecret = (): void => {
    secretRef.current?.fill(0)
    secretRef.current = null
  }

  useEffect(() => {
    let alive = true
    let stopCamera: (() => void) | null = null
    let timer: ReturnType<typeof setInterval> | null = null
    let reading = false
    const stopAll = (): void => {
      if (timer) clearInterval(timer)
      timer = null
      stopCamera?.()
      stopCamera = null
    }

    const accept = async (nodeId: string, enrollId: string, secret: Uint8Array): Promise<void> => {
      // 认出来了：立刻停摄像头，不再读画面
      stopAll()
      secretRef.current = secret
      setView({ kind: 'loading' })
      const infos = await refreshEnrolls(nodeId)
      if (!alive) return
      if (infos === null) {
        wipeSecret()
        setView({ kind: 'failed', message: t('scan.offline') })
        return
      }
      const info = infos.find((i) => i?.enroll_id === enrollId)
      if (!info) {
        wipeSecret()
        setView({ kind: 'missing' })
        return
      }
      // 只有「扫码请求且在等批准」才给确认页：扫到「请求批准」的二维码也能走到 `node.enroll_approve`，
      // daemon 会忽略密钥、返回 6 位码，这里又不显示它，那个请求就白白作废了
      if (info.method !== 'scan' || info.status !== 'wait_approval') {
        wipeSecret()
        setView({ kind: info.method && info.method !== 'scan' ? 'notScan' : 'notWaiting' })
        return
      }
      setView({ kind: 'confirm', nodeId, enrollId, info })
    }

    const onText = (text: string): void => {
      const parsed = parseEnrollQr(text)
      if (!parsed) {
        setNotice((n) => (n?.kind === 'notAnycon' ? n : { kind: 'notAnycon' }))
        return
      }
      if (!useNodes.getState().byId[parsed.nodeId]) {
        parsed.secret.fill(0)
        setNotice((n) => (n?.kind === 'unpaired' && n.node === parsed.nodeId.slice(0, 8) ? n : { kind: 'unpaired', node: parsed.nodeId.slice(0, 8) }))
        return
      }
      setNotice(null)
      void accept(parsed.nodeId, parsed.enrollId, parsed.secret)
    }

    void (async () => {
      setView({ kind: 'starting' })
      setNotice(null)
      try {
        const video = videoRef.current
        if (!video) return
        const stop = await openCamera(video)
        if (!alive) {
          stop()
          return
        }
        stopCamera = stop
        const reader = await createQrReader()
        if (!alive) return
        setView({ kind: 'scanning' })
        timer = setInterval(() => {
          if (reading || !alive) return
          reading = true
          reader
            .read(video)
            .then((text) => {
              if (text && alive && timer) onText(text)
            })
            .catch((e: unknown) => console.warn('anycon: could not decode a camera frame', e))
            .finally(() => {
              reading = false
            })
        }, SCAN_INTERVAL_MS)
      } catch (e) {
        if (!alive) return
        setView({ kind: e instanceof CameraError && e.code === 'denied' ? 'denied' : 'unavailable' })
      }
    })()

    return () => {
      alive = false
      stopAll()
      wipeSecret()
    }
    // `attempt` 变了 = 用户点了「重新扫描」/「再试一次」
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [attempt])

  const approve = async (v: Extract<View, { kind: 'confirm' }>): Promise<void> => {
    const secret = secretRef.current
    if (!secret) return
    setView({ kind: 'approving', nodeId: v.nodeId, enrollId: v.enrollId, info: v.info })
    try {
      await approveScan(v.nodeId, v.enrollId, hexOf(secret))
      wipeSecret()
      setView({ kind: 'done' })
    } catch (e) {
      wipeSecret()
      if (isAlreadyResolved(e)) setView({ kind: 'failed', message: t('scan.taken') })
      else if (isGone(e)) setView({ kind: 'failed', message: t('scan.gone') })
      else setView({ kind: 'failed', message: describeError(e, t) })
    }
  }

  const rescan = (): void => {
    wipeSecret()
    setAttempt((n) => n + 1)
  }

  const showVideo = view.kind === 'starting' || view.kind === 'scanning'
  return (
    <div className="space-y-4" data-testid="scan-session" data-view={view.kind}>
      {/* 画面只在本地解码：不上传、不录制、不截图 */}
      <div className={showVideo ? 'relative overflow-hidden rounded-lg bg-black' : 'hidden'}>
        <video ref={videoRef} className="aspect-square max-h-[60dvh] w-full object-cover" playsInline muted aria-label={t('scan.camera')} data-testid="scan-video" />
        {view.kind === 'starting' ? (
          <div className="absolute inset-0 flex items-center justify-center text-white">
            <Spinner className="text-white" />
          </div>
        ) : (
          <ScanLine className="pointer-events-none absolute inset-8 size-auto text-white/70" aria-hidden strokeWidth={0.75} />
        )}
      </div>

      {view.kind === 'scanning' || view.kind === 'starting' ? <p className="text-sm text-fg-muted">{t('scan.hint')}</p> : null}
      {showVideo && notice ? (
        <p className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning-subtle p-2 text-sm text-warning" role="alert" data-testid="scan-notice" data-kind={notice.kind}>
          <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
          {notice.kind === 'notAnycon' ? t('scan.notAnycon') : t('scan.unpaired', { node: notice.node })}
        </p>
      ) : null}

      {view.kind === 'denied' ? (
        <div className="space-y-3" data-testid="scan-denied">
          <p className="flex items-start gap-2 text-sm text-danger">
            <CameraOff className="mt-0.5 size-4 shrink-0" aria-hidden />
            {t('scan.denied')}
          </p>
          <p className="text-sm text-fg-muted">{t('scan.deniedHelp')}</p>
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={onClose}>
              {t('common.close')}
            </Button>
            <Button variant="primary" onClick={rescan} data-testid="scan-retry">
              {t('scan.retry')}
            </Button>
          </div>
        </div>
      ) : null}
      {view.kind === 'unavailable' ? (
        <div className="space-y-3" data-testid="scan-unavailable">
          <p className="flex items-start gap-2 text-sm text-danger">
            <CameraOff className="mt-0.5 size-4 shrink-0" aria-hidden />
            {t('scan.unavailable')}
          </p>
          <div className="flex justify-end">
            <Button variant="outline" onClick={onClose}>
              {t('common.close')}
            </Button>
          </div>
        </div>
      ) : null}

      {view.kind === 'loading' ? (
        <p className="flex items-center gap-2 text-sm text-fg-muted">
          <Spinner />
          {t('scan.loading')}
        </p>
      ) : null}

      {view.kind === 'confirm' || view.kind === 'approving' ? <Confirm view={view} onApprove={(v) => void approve(v)} onCancel={onClose} nodeName={nodes[view.nodeId]?.name ?? ''} /> : null}

      {view.kind === 'missing' ? (
        <div className="space-y-3" data-testid="scan-missing">
          <p className="text-sm text-fg-muted">{t('scan.missing')}</p>
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={onClose}>
              {t('common.close')}
            </Button>
            <Button variant="primary" onClick={rescan}>
              {t('scan.again')}
            </Button>
          </div>
        </div>
      ) : null}

      {view.kind === 'notScan' || view.kind === 'notWaiting' ? (
        <div className="space-y-3" data-testid={view.kind === 'notScan' ? 'scan-not-scan' : 'scan-not-waiting'}>
          <p className="flex items-start gap-2 text-sm text-warning" role="alert">
            <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
            {view.kind === 'notScan' ? t('scan.notScan') : t('scan.notWaiting')}
          </p>
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={onClose}>
              {t('common.close')}
            </Button>
            <Button variant="primary" onClick={rescan}>
              {t('scan.again')}
            </Button>
          </div>
        </div>
      ) : null}

      {view.kind === 'done' ? (
        <div className="space-y-3" data-testid="scan-done">
          <p className="flex items-center gap-2 text-sm text-success">
            <Check className="size-4" aria-hidden />
            {t('scan.done')}
          </p>
          <div className="flex justify-end">
            <Button variant="primary" onClick={onClose}>
              {t('common.close')}
            </Button>
          </div>
        </div>
      ) : null}

      {view.kind === 'failed' ? (
        <div className="space-y-3" data-testid="scan-failed">
          <p className="text-sm text-danger" role="alert">
            {view.message}
          </p>
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={onClose}>
              {t('common.close')}
            </Button>
            <Button variant="primary" onClick={rescan}>
              {t('scan.again')}
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  )
}

function Confirm({ view, nodeName, onApprove, onCancel }: { view: Extract<View, { kind: 'confirm' | 'approving' }>; nodeName: string; onApprove: (v: Extract<View, { kind: 'confirm' }>) => void; onCancel: () => void }) {
  const { t } = useTranslation()
  const hint = displayHint(view.info.hint)
  return (
    <div className="space-y-3" data-testid="scan-confirm">
      <p className="text-sm">{t('scan.confirmNode', { node: nodeName })}</p>
      <div>
        <p className="font-medium" data-testid="scan-hint-name">
          {hint.name ?? t('approve.unknownDevice')}
        </p>
        <p className="text-xs text-fg-subtle">
          {hint.platform ? `${hint.platform} · ` : ''}
          {t('approve.unverified')}
        </p>
      </div>
      <p className="text-sm font-medium">{t('scan.onlyIfInFront')}</p>
      <div className="flex justify-end gap-2">
        <Button variant="outline" onClick={onCancel} disabled={view.kind === 'approving'} data-testid="scan-cancel">
          {t('common.cancel')}
        </Button>
        <Button variant="primary" loading={view.kind === 'approving'} onClick={() => view.kind === 'confirm' && onApprove(view)} data-testid="scan-approve">
          {t('approve.approve')}
        </Button>
      </div>
    </div>
  )
}
