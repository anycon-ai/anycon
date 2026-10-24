import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from 'react'
import { AlertTriangle, ExternalLink, Globe, RefreshCw, Square } from 'lucide-react'
import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import type { PreviewPort } from '@/protocol'
import { describeError } from '@/lib/errors'
import { useIsWide } from '@/lib/useMediaQuery'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Input, Select } from '@/ui/Field'
import { Card, EmptyState, Spinner } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'
import { describePreviewError } from './errors'
import { closePreview, ensureTicket, knownTicket, launchUrl, normalizePreviewPath, usableTicket } from './ticket'
import { usePreviewPorts } from './usePreviewPorts'

/** iframe 的 sandbox（web.md § 13.1）。**不给** `allow-top-navigation`：预览页不能把 Anycon 自己导航走。 */
const SANDBOX = 'allow-scripts allow-same-origin allow-forms allow-popups allow-popups-to-escape-sandbox allow-modals allow-downloads'

/** 端口下拉里「其他端口…」那一项的值。 */
const OTHER = 'other'

/** 每个会话最近选的端口，切走再切回来（视图卸载了）时接着用。只在内存里。 */
const lastPort = new Map<string, number>()

interface Frame {
  /** 每次（重新）打开都换，让 iframe 重新挂载：只有 fragment 不同的 URL 不会触发导航。 */
  key: number
  port: number
  /** 含票据密钥和令牌的 URL：只给 iframe 的 `src`，不写日志。 */
  url: string
}

type Phase = 'idle' | 'opening' | 'ready' | 'stopped'

function portLabel(port: number, info: PreviewPort | undefined, t: TFunction): string {
  const parts = [String(port)]
  if (info?.process) parts.push(info.process)
  if (info?.source === 'config') parts.push(t('preview.sourceConfig'))
  return parts.join(' · ')
}

/** 「其他端口」输入框里的内容 → 端口号；不是 1–65535 的整数返回 null。 */
function parsePort(text: string): number | null {
  if (!/^\d{1,5}$/.test(text.trim())) return null
  const n = Number(text.trim())
  return n >= 1 && n <= 65535 ? n : null
}

export interface PreviewViewProps {
  nodeId: string
  sessionId: string
  /** `/api/config.preview_origin_template`。 */
  template: string
  /** 和节点的连接没就绪：不再轮询端口，要问 daemon 的操作禁用。 */
  offline: boolean
}

/**
 * 会话页的「预览」主视图（docs/spec/web.md § 13.1，ADR-39）：工具条（端口、地址栏、刷新、在新标签页打开、停止）加一个 iframe。
 * 视图开着时每 5 秒更新端口列表；选一个端口就 `preview.open` → 向 hub 要令牌 → 把启动参数放进 URL fragment 交给预览页。
 * 拿到的票据只缓存在内存里（`ticket.ts`）；不监听 iframe 发来的 `message`。
 */
export function PreviewView({ nodeId, sessionId, template, offline }: PreviewViewProps) {
  const { t } = useTranslation()
  const wide = useIsWide()
  const ports = usePreviewPorts(nodeId, sessionId, !offline)
  const [selected, setSelected] = useState<number | null>(null)
  /** 用户手输过、不在端口列表里的端口：留在下拉里，不然选中的值没有对应的项。 */
  const [extra, setExtra] = useState<number[]>([])
  const [otherOpen, setOtherOpen] = useState(false)
  const [otherText, setOtherText] = useState('')
  const [otherInvalid, setOtherInvalid] = useState(false)
  const [pathText, setPathText] = useState('/')
  const [path, setPath] = useState('/')
  const [frame, setFrame] = useState<Frame | null>(null)
  const [phase, setPhase] = useState<Phase>('idle')
  const [error, setError] = useState<unknown>(null)
  const [tabBusy, setTabBusy] = useState(false)
  const [stopOpen, setStopOpen] = useState(false)
  const [stopping, setStopping] = useState(false)
  const attempt = useRef(0)
  const frameSeq = useRef(0)
  const autoPicked = useRef(false)

  // 卸载后飞着的请求不再落地
  useEffect(
    () => () => {
      attempt.current += 1
    },
    [],
  )

  /** 取票据并把 iframe 指向 `toPath`；后发起的请求让先发起的作废（连续切端口）。 */
  const open = useCallback(
    async (port: number, toPath: string): Promise<void> => {
      const mine = ++attempt.current
      setPhase('opening')
      setError(null)
      try {
        const ticket = await ensureTicket({ nodeId, port, sessionId, template })
        if (mine !== attempt.current) return
        frameSeq.current += 1
        setFrame({ key: frameSeq.current, port, url: launchUrl(ticket, toPath) })
        setPhase('ready')
      } catch (e) {
        if (mine !== attempt.current) return
        console.warn('anycon: could not open the preview', e)
        setFrame(null)
        setError(e)
        setPhase('idle')
      }
    },
    [nodeId, sessionId, template],
  )

  const choosePort = useCallback(
    (port: number): void => {
      lastPort.set(sessionId, port)
      setSelected(port)
      setPath('/')
      setPathText('/')
      setOtherOpen(false)
      if (!ports.ports.some((p) => p.port === port)) setExtra((prev) => (prev.includes(port) ? prev : [...prev, port]))
      void open(port, '/')
    },
    [sessionId, ports.ports, open],
  )

  // 第一次拿到端口列表、用户还没选过：选上次用的；没有就选最小的正在监听的端口（`config` 里的端口不一定在跑，排在后面）并打开
  useEffect(() => {
    if (autoPicked.current || selected !== null || ports.status !== 'ready' || ports.ports.length === 0) return
    autoPicked.current = true
    const remembered = lastPort.get(sessionId)
    choosePort((ports.ports.find((p) => p.port === remembered) ?? ports.ports.find((p) => p.source !== 'config') ?? ports.ports[0]!).port)
  }, [selected, ports.status, ports.ports, sessionId, choosePort])

  const options = useMemo(() => {
    const listed = new Map(ports.ports.map((p) => [p.port, p]))
    const all = new Set<number>([...listed.keys(), ...extra, ...(selected === null ? [] : [selected])])
    return [...all].sort((a, b) => a - b).map((port) => ({ port, label: portLabel(port, listed.get(port), t) }))
  }, [ports.ports, extra, selected, t])

  const known = selected === null ? undefined : knownTicket(nodeId, selected)
  const opening = phase === 'opening'

  const submitPath = (e: FormEvent): void => {
    e.preventDefault()
    if (selected === null) return
    const next = normalizePreviewPath(pathText)
    setPath(next)
    setPathText(next)
    void open(selected, next)
  }

  const submitOther = (e: FormEvent): void => {
    e.preventDefault()
    const port = parsePort(otherText)
    if (port === null) {
      setOtherInvalid(true)
      return
    }
    setOtherInvalid(false)
    setOtherText('')
    choosePort(port)
  }

  const reload = (): void => {
    if (selected === null) return
    ports.refresh()
    void open(selected, path)
  }

  /** 缓存里的票据够新就同步地 `window.open`（保住点击带来的用户激活，弹窗不被拦）；否则先取票据。 */
  const openTab = async (): Promise<void> => {
    if (selected === null) return
    const cached = usableTicket(nodeId, selected)
    if (cached) {
      window.open(launchUrl(cached, path), '_blank', 'noopener,noreferrer')
      return
    }
    setTabBusy(true)
    setError(null)
    try {
      const ticket = await ensureTicket({ nodeId, port: selected, sessionId, template })
      window.open(launchUrl(ticket, path), '_blank', 'noopener,noreferrer')
    } catch (e) {
      console.warn('anycon: could not open the preview in a new tab', e)
      setError(e)
    } finally {
      setTabBusy(false)
    }
  }

  const stop = async (): Promise<void> => {
    if (!known || selected === null) return
    setStopping(true)
    try {
      await closePreview(nodeId, known.label)
      attempt.current += 1
      setFrame(null)
      setError(null)
      setPhase('stopped')
      setStopOpen(false)
      toast.success(t('preview.stoppedToast', { port: selected }))
    } catch (e) {
      toast.error(describePreviewError(e, t))
    } finally {
      setStopping(false)
    }
  }

  const retry = (): void => {
    if (selected !== null) void open(selected, path)
  }

  const portsFailed = ports.status === 'error' && ports.ports.length === 0

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="preview-view">
      <div className="flex shrink-0 items-center gap-1.5 border-b border-border bg-bg px-2 py-1.5 md:gap-2 md:px-4" role="toolbar" aria-label={t('preview.toolbar')} data-testid="preview-toolbar">
        <div className="max-w-[7rem] shrink-0 md:max-w-[18rem]">
          <Select
            aria-label={t('preview.port')}
            data-testid="preview-port"
            value={otherOpen ? OTHER : selected === null ? '' : String(selected)}
            disabled={offline}
            onChange={(e) => {
              if (e.target.value === OTHER) setOtherOpen(true)
              else choosePort(Number(e.target.value))
            }}
          >
            {selected === null && !otherOpen ? (
              <option value="" disabled>
                {t('preview.choosePort')}
              </option>
            ) : null}
            {options.map((o) => (
              <option key={o.port} value={String(o.port)}>
                {o.label}
              </option>
            ))}
            <option value={OTHER}>{t('preview.otherPort')}</option>
          </Select>
        </div>
        <form className="min-w-0 flex-1" onSubmit={submitPath}>
          <Input
            aria-label={t('preview.path')}
            data-testid="preview-path"
            value={pathText}
            onChange={(e) => setPathText(e.target.value)}
            disabled={selected === null}
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            inputMode="url"
            className="font-mono"
          />
        </form>
        <Button variant="ghost" size="icon" aria-label={t('preview.reload')} title={t('preview.reload')} onClick={reload} disabled={selected === null || opening} data-testid="preview-reload">
          <RefreshCw className="size-4" />
        </Button>
        {/* 手机上浏览器对第三方 iframe 里的 service worker 支持参差：「在新标签页打开」是最显眼的按钮 */}
        <Button
          variant={wide ? 'outline' : 'primary'}
          size={wide ? 'icon' : 'md'}
          aria-label={t('preview.openTab')}
          title={t('preview.openTab')}
          icon={<ExternalLink className="size-4" />}
          loading={tabBusy}
          onClick={() => void openTab()}
          disabled={selected === null}
          data-testid="preview-open-tab"
        >
          {wide ? null : t('preview.openTabShort')}
        </Button>
        <Button
          variant="ghost"
          size="icon"
          className="text-danger"
          aria-label={t('preview.stop')}
          title={t('preview.stop')}
          onClick={() => setStopOpen(true)}
          disabled={!known || offline}
          data-testid="preview-stop"
        >
          <Square className="size-4" />
        </Button>
      </div>

      {otherOpen ? (
        <form className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border bg-bg-subtle px-2 py-1.5 md:px-4" onSubmit={submitOther} data-testid="preview-other">
          <label htmlFor="preview-other-port" className="text-xs font-medium text-fg-muted">
            {t('preview.otherLabel')}
          </label>
          <div className="w-28">
            <Input
              id="preview-other-port"
              data-testid="preview-other-port"
              value={otherText}
              onChange={(e) => {
                setOtherText(e.target.value)
                setOtherInvalid(false)
              }}
              inputMode="numeric"
              autoComplete="off"
              autoFocus
              aria-invalid={otherInvalid}
              placeholder="3000"
              className="font-mono"
            />
          </div>
          <Button type="submit" variant="primary" disabled={otherText.trim() === '' || offline} data-testid="preview-other-open">
            {t('preview.otherOpen')}
          </Button>
          {otherInvalid ? (
            <span className="text-xs text-danger" role="alert" data-testid="preview-other-invalid">
              {t('preview.otherInvalid')}
            </span>
          ) : null}
        </form>
      ) : null}

      {offline ? <Notice testId="preview-offline">{t('preview.offline')}</Notice> : null}
      {ports.detection === 'unavailable' ? <Notice testId="preview-detection-hint">{t('preview.detectionUnavailable')}</Notice> : null}
      {portsFailed ? <Notice testId="preview-ports-error">{t('preview.portsError', { error: describeError(ports.error, t) })}</Notice> : null}
      {error !== null && frame ? (
        <Notice testId="preview-error" tone="danger">
          {describePreviewError(error, t)}
        </Notice>
      ) : null}

      <div className="relative min-h-0 flex-1 bg-bg-subtle">
        {frame ? (
          <iframe
            key={frame.key}
            src={frame.url}
            title={t('preview.frameTitle', { port: frame.port })}
            sandbox={SANDBOX}
            referrerPolicy="no-referrer"
            className="absolute inset-0 size-full border-0 bg-white"
            data-testid="preview-frame"
          />
        ) : opening ? (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-fg-subtle" role="status" data-testid="preview-opening">
            <Spinner /> {t('preview.opening')}
          </div>
        ) : error !== null ? (
          <div className="flex h-full items-center justify-center p-4" data-testid="preview-error">
            <EmptyState
              icon={<AlertTriangle className="size-6 text-danger" aria-hidden />}
              title={t('preview.errorTitle')}
              action={
                <Button variant="outline" icon={<RefreshCw className="size-4" />} onClick={retry} disabled={selected === null} data-testid="preview-retry">
                  {t('preview.retry')}
                </Button>
              }
            >
              <span role="alert">{describePreviewError(error, t)}</span>
            </EmptyState>
          </div>
        ) : phase === 'stopped' && selected !== null ? (
          <div className="flex h-full items-center justify-center p-4" data-testid="preview-stopped">
            <EmptyState
              icon={<Square className="size-6" aria-hidden />}
              title={t('preview.stoppedTitle')}
              action={
                <Button variant="primary" onClick={retry} disabled={offline} data-testid="preview-restart">
                  {t('preview.restart')}
                </Button>
              }
            >
              {t('preview.stoppedBody', { port: selected })}
            </EmptyState>
          </div>
        ) : ports.status === 'loading' && !offline ? (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-fg-subtle" role="status" data-testid="preview-loading">
            <Spinner /> {t('preview.loading')}
          </div>
        ) : selected === null ? (
          <div className="flex h-full items-center justify-center p-4" data-testid="preview-empty">
            <EmptyState icon={<Globe className="size-6" aria-hidden />} title={t('preview.emptyTitle')}>
              {t('preview.emptyBody')}
            </EmptyState>
          </div>
        ) : null}
      </div>

      <Dialog
        open={stopOpen}
        onOpenChange={(v) => !v && !stopping && setStopOpen(false)}
        title={t('preview.stopTitle', { port: selected ?? '' })}
        description={t('preview.stopBody')}
        closeLabel={t('common.close')}
        testId="preview-stop-dialog"
      >
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setStopOpen(false)} disabled={stopping}>
            {t('common.cancel')}
          </Button>
          <Button variant="danger" loading={stopping} onClick={() => void stop()} data-testid="preview-stop-confirm">
            {t('preview.stop')}
          </Button>
        </div>
      </Dialog>
    </div>
  )
}

function Notice({ children, testId, tone }: { children: string; testId: string; tone?: 'danger' }) {
  return (
    <Card tone={tone ?? 'warning'} className="mx-2 mt-2 shrink-0 px-3 py-2 text-xs md:mx-4" testId={testId}>
      <p role={tone === 'danger' ? 'alert' : 'status'}>{children}</p>
    </Card>
  )
}
