import { useCallback, useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react'
import { Accordion } from 'radix-ui'
import { ChevronDown, KeyRound, Lock, Pencil, QrCode as QrIcon, Send, Smartphone } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { ApiDevice } from '@/api/types'
import { describeError } from '@/lib/errors'
import { formatCountdown } from '@/lib/expiry'
import { useNow } from '@/lib/useNow'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { getClient } from '@/transport/instance'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Field, Input } from '@/ui/Field'
import { Spinner } from '@/ui/Misc'
import { QrCode } from '@/ui/QrCode'
import { CodeInput } from './CodeInput'
import type { UnlockEntry } from './errors'
import { backoffLeft, UnlockError } from './UnlockError'
import { UnlockProgress } from './UnlockProgress'
import { useUnlock, type UnlockState } from './useUnlock'

const ICONS: Record<UnlockEntry, typeof QrIcon> = { scan: QrIcon, approve: Smartphone, code: KeyRound, password: Lock }
const ENTRIES: UnlockEntry[] = ['scan', 'approve', 'code', 'password']

/**
 * 解锁一台节点的四个入口（spec/web.md § 4.3.1）：用已配对的设备扫码（默认展开）/ 在已配对设备上批准 / 我有接入码 / 节点口令。
 * 同一时刻只进行一个：换入口、离开页面都会取消（`enroll_cancel`）。节点离线时全部禁用，上线后自动恢复。
 * 四个入口都由用户操作才开始（扫码入口点「显示二维码」）：每个接入会话都占着 hub 的限频和 daemon 的名额，打开页面不该就占一个。
 */
export function UnlockPanel({ node, online, onDone, onNotFound }: { node: ApiDevice; online: boolean; onDone: (nodeId: string) => void; onNotFound?: () => void }) {
  const { t } = useTranslation()
  const hubName = useHub((s) => s.snapshot.deviceName)
  // `null` = 没改过，用默认名；改的时候输入框显示用户正在输入的（清空了就回落到默认名发出去）
  const [name, setName] = useState<string | null>(null)
  const [editingName, setEditingName] = useState(false)
  const defaultName = hubName || getClient().deviceName
  const deviceName = name?.trim() || defaultName
  const [open, setOpen] = useState<UnlockEntry>('scan')
  const u = useUnlock(node.id, deviceName, onDone)
  const s = u.state
  const busy = s.phase !== 'idle' && s.phase !== 'error' && s.phase !== 'done'

  // hub 说「不是你账号的节点」：交给页面显示 404
  const notFound = s.error?.code === 'not_found'
  useEffect(() => {
    if (notFound) onNotFound?.()
  }, [notFound, onNotFound])

  // 二维码发出去时带的设备名（hint）；二维码在显示期间改了名，就换一个新的
  const { start } = u
  const nameRef = useRef(deviceName)
  nameRef.current = deviceName
  const scanName = useRef(deviceName)
  const startScan = useCallback(() => {
    scanName.current = nameRef.current
    void start({ entry: 'scan' })
  }, [start])

  const choose = (value: string): void => {
    const next = value as UnlockEntry
    if (!ENTRIES.includes(next) || next === open) return
    setOpen(next)
    void u.cancel()
  }

  return (
    <div className="space-y-4" data-testid="unlock-panel">
      {!online ? (
        <p className="rounded-md border border-warning/40 bg-warning-subtle p-3 text-sm text-warning" role="status" data-testid="unlock-offline">
          {t('unlock.offline')}
        </p>
      ) : null}

      <div className="text-sm">
        {editingName ? (
          <Field label={t('pair.deviceName')} htmlFor="unlock-device-name">
            <Input
              id="unlock-device-name"
              data-testid="unlock-device-name"
              value={name ?? defaultName}
              onChange={(e) => setName(e.target.value)}
              // 改完名字，正在显示的二维码里的 hint 就是旧名字了：换一个新的（没显示二维码就什么都不做，下次点按钮用新名字）
              onBlur={() => {
                if (online && s.entry === 'scan' && busy && scanName.current !== nameRef.current) startScan()
              }}
              disabled={busy && s.entry !== 'scan'}
              maxLength={80}
            />
          </Field>
        ) : (
          <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-fg-muted">
            <span>{t('pair.deviceAs', { name: deviceName })}</span>
            <button type="button" className="inline-flex shrink-0 items-center gap-1 whitespace-nowrap text-accent-text hover:underline disabled:opacity-50" onClick={() => { setName(deviceName); setEditingName(true) }} disabled={busy && s.entry !== 'scan'}>
              <Pencil className="size-3" />
              {t('pair.rename')}
            </button>
          </p>
        )}
      </div>

      <Accordion.Root type="single" value={open} onValueChange={choose} className="divide-y divide-border overflow-hidden rounded-lg border border-border bg-surface">
        {ENTRIES.map((id) => {
          const Icon = ICONS[id]
          return (
            <Accordion.Item key={id} value={id} disabled={!online} data-testid={`unlock-entry-${id}`} className="data-[disabled]:opacity-60">
              <Accordion.Header className="flex">
                <Accordion.Trigger data-testid={`unlock-trigger-${id}`} className="group flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-bg-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent/40 data-[disabled]:cursor-not-allowed">
                  <Icon className="size-5 shrink-0 text-accent-text" aria-hidden />
                  <span className="min-w-0 flex-1">
                    <span className="block text-sm font-medium">{t(`unlock.entries.${id}.title`)}</span>
                    <span className="block text-xs text-fg-subtle">{t(`unlock.entries.${id}.hint`)}</span>
                  </span>
                  <ChevronDown className="size-4 shrink-0 text-fg-subtle transition-transform group-data-[state=open]:rotate-180" aria-hidden />
                </Accordion.Trigger>
              </Accordion.Header>
              <Accordion.Content className="space-y-3 border-t border-border bg-bg-subtle px-4 py-4">
                <EntryBody id={id} s={s.entry === id ? s : null} online={online} busy={busy} nodeId={node.id} u={u} startScan={startScan} />
              </Accordion.Content>
            </Accordion.Item>
          )
        })}
      </Accordion.Root>
    </div>
  )
}

type Unlock = ReturnType<typeof useUnlock>

function EntryBody({ id, s, online, busy, nodeId, u, startScan }: { id: UnlockEntry; s: UnlockState | null; online: boolean; busy: boolean; nodeId: string; u: Unlock; startScan: () => void }) {
  const { t } = useTranslation()
  const running = !!s && s.phase !== 'idle' && s.phase !== 'error' && s.phase !== 'done'
  const restart = (): void => {
    // 能自己重来的入口（扫码、请求批准）一键重来；接入码和口令要用户再输一次
    if (id === 'scan') startScan()
    else if (id === 'approve') void u.start({ entry: id })
    else void u.cancel()
  }
  const error = s?.phase === 'error' ? s.error : null
  const known = useNodes((st) => st.byId[nodeId] !== undefined)
  // 本机已经有这个节点、但它的公钥变了（重装了 daemon）：不覆盖，先忘记旧的
  const forget = error?.code === 'key_changed' && known

  return (
    <>
      {id === 'scan' ? <ScanBody s={s} online={online} onShow={startScan} /> : null}
      {id === 'approve' ? <ApproveBody s={s} online={online} busy={busy} u={u} /> : null}
      {id === 'code' ? <CodeBody s={s} online={online} busy={busy} u={u} /> : null}
      {id === 'password' ? <PasswordBody s={s} online={online} busy={busy} u={u} /> : null}

      {running && s?.phase !== 'connecting' && id !== 'scan' ? (
        <Button variant="ghost" size="sm" onClick={() => void u.cancel()} data-testid="unlock-cancel">
          {t('common.cancel')}
        </Button>
      ) : null}
      {s && s.phase !== 'idle' && s.phase !== 'error' && s.phase !== 'done' && s.phase !== 'waiting' ? <UnlockProgress entry={id} phase={s.phase} /> : null}
      {error && s ? <UnlockError error={error} errorAt={s.errorAt} entry={id} onRestart={restart} /> : null}
      {forget ? (
        <Button
          size="sm"
          variant="outline"
          data-testid="unlock-forget"
          onClick={() => {
            getClient()
              .removeNode(nodeId)
              .then(() => useNodes.getState().remove(nodeId))
              .catch((e: unknown) => toast.error(describeError(e, t)))
          }}
        >
          {t('node.forget')}
        </Button>
      ) : null}
    </>
  )
}

function ScanBody({ s, online, onShow }: { s: UnlockState | null; online: boolean; onShow: () => void }) {
  const { t } = useTranslation()
  const showing = s?.phase === 'waiting' && s.qr
  const now = useNow(1000, !!showing)
  const failed = s?.phase === 'error'
  // 还没开始（或页面进后台时被取消了）：等用户点按钮
  const idle = !s || s.phase === 'idle'
  const paused = !!s?.paused
  return (
    <div className="space-y-3" data-testid="unlock-scan">
      <p className="text-sm text-fg-muted">{t('unlock.scan.how')}</p>
      {showing && s.qr ? (
        <div className="flex flex-col items-center gap-2" data-testid="unlock-qr">
          <QrCode text={s.qr} size={208} label={t('unlock.scan.qrLabel')} />
          <p className="text-xs text-fg-subtle" data-testid="unlock-countdown">
            {t('unlock.scan.expires', { time: formatCountdown((s.expiresAt ?? now) - now) })}
          </p>
        </div>
      ) : idle ? (
        <div className="space-y-3">
          {paused ? (
            <p className="text-sm text-fg-muted" data-testid="unlock-scan-paused">
              {t('unlock.scan.paused')}
            </p>
          ) : null}
          <Button variant="primary" icon={<QrIcon className="size-4" />} disabled={!online} onClick={onShow} data-testid="unlock-show-qr">
            {paused ? t('unlock.scan.reshow') : t('unlock.scan.show')}
          </Button>
        </div>
      ) : failed ? null : (
        <p className="flex items-center gap-2 text-sm text-fg-muted" data-testid="unlock-scan-preparing">
          <Spinner />
          {t('unlock.scan.preparing')}
        </p>
      )}
    </div>
  )
}

function ApproveBody({ s, online, busy, u }: { s: UnlockState | null; online: boolean; busy: boolean; u: Unlock }) {
  const { t } = useTranslation()
  const [code, setCode] = useState('')
  const phase = s?.phase
  const active = !!s && phase !== 'error' && phase !== 'done' && phase !== 'idle'
  const asking = phase === 'code'
  // 输错之后输入框清空，让用户再输
  useEffect(() => {
    if (asking && s?.wrongCode) setCode('')
  }, [asking, s?.wrongCode, s?.attemptsLeft])
  return (
    <div className="space-y-3" data-testid="unlock-approve">
      <p className="text-sm text-fg-muted">{active ? t('unlock.approve.waiting') : t('unlock.approve.intro')}</p>
      {!active ? (
        <Button variant="primary" icon={<Send className="size-4" />} disabled={!online || busy} onClick={() => void u.start({ entry: 'approve' })} data-testid="unlock-send-request">
          {t('unlock.approve.send')}
        </Button>
      ) : (
        <CodeEntry value={code} onChange={setCode} onComplete={(d) => u.submitCode(d)} disabled={!asking} wrong={asking && !!s?.wrongCode} attemptsLeft={s?.attemptsLeft ?? null} testId="unlock-approve-code" />
      )}
    </div>
  )
}

function CodeBody({ s, online, busy, u }: { s: UnlockState | null; online: boolean; busy: boolean; u: Unlock }) {
  const { t } = useTranslation()
  const [code, setCode] = useState('')
  const asking = s?.phase === 'code'
  useEffect(() => {
    if (asking && s?.wrongCode) setCode('')
  }, [asking, s?.wrongCode, s?.attemptsLeft])
  // 重试（同一个会话里再问一次）时直接回答；其余时候是一次新的接入
  const submit = (digits: string): void => {
    if (asking) u.submitCode(digits)
    else void u.start({ entry: 'code', code: digits })
  }
  const editable = online && (!busy || (asking && !!s?.wrongCode))
  return (
    <div className="space-y-3" data-testid="unlock-code">
      <p className="text-sm text-fg-muted">{t('unlock.code.intro')}</p>
      <form
        className="space-y-3"
        onSubmit={(e: FormEvent) => {
          e.preventDefault()
          if (code.length === 6 && editable) submit(code)
        }}
      >
        <CodeEntry value={code} onChange={setCode} onComplete={editable ? submit : undefined} disabled={!editable} wrong={asking && !!s?.wrongCode} attemptsLeft={s?.attemptsLeft ?? null} testId="unlock-code-input" />
        <Button type="submit" variant="primary" className="w-full" disabled={!editable || code.length !== 6} loading={busy && !asking} data-testid="unlock-code-submit">
          {t('unlock.code.submit')}
        </Button>
      </form>
    </div>
  )
}

function CodeEntry({ value, onChange, onComplete, disabled, wrong, attemptsLeft, testId }: { value: string; onChange: (d: string) => void; onComplete?: (d: string) => void; disabled: boolean; wrong: boolean; attemptsLeft: number | null; testId: string }) {
  const { t } = useTranslation()
  return (
    <div className="space-y-1.5">
      <CodeInput value={value} onChange={onChange} onComplete={onComplete} disabled={disabled} invalid={wrong} label={t('unlock.code.label')} testId={testId} />
      {wrong ? (
        <p className="text-xs text-danger" role="alert" data-testid="unlock-wrong-code">
          {t('unlock.errors.wrongCode')}
          {attemptsLeft !== null ? ` ${t('unlock.code.attempts', { count: attemptsLeft })}` : ''}
        </p>
      ) : null}
    </div>
  )
}

function PasswordBody({ s, online, busy, u }: { s: UnlockState | null; online: boolean; busy: boolean; u: Unlock }) {
  const { t } = useTranslation()
  const [password, setPassword] = useState('')
  // 退避期间提交按钮禁用，倒计时走完自动恢复
  const now = useNow(1000, s?.error?.code === 'backoff')
  const waiting = backoffLeft(s?.error ?? null, s?.errorAt ?? null, now) > 0
  const submit = (e: FormEvent): void => {
    e.preventDefault()
    if (!password || busy || waiting) return
    void u.start({ entry: 'password', password })
    // 口令用完就丢：交给传输层之后输入框立刻清空，不在组件状态里多留（输错了要重新输）
    setPassword('')
  }
  const field: ReactNode = (
    <Input
      id="unlock-password"
      data-testid="unlock-password-input"
      type="password"
      value={password}
      onChange={(e) => setPassword(e.target.value)}
      // 借来的电脑上不要让浏览器存口令；anycon 自己从不保存
      autoComplete="off"
      autoCapitalize="off"
      autoCorrect="off"
      spellCheck={false}
      data-1p-ignore
      data-lpignore="true"
      disabled={!online || busy}
      className={cn('h-11')}
    />
  )
  return (
    <form className="space-y-3" onSubmit={submit} data-testid="unlock-password">
      <p className="text-sm text-fg-muted">{t('unlock.password.intro')}</p>
      <Field label={t('unlock.password.label')} htmlFor="unlock-password" hint={t('unlock.password.hint')}>
        {field}
      </Field>
      <Button type="submit" variant="primary" className="w-full" disabled={!online || !password || waiting} loading={busy} data-testid="unlock-password-submit">
        {t('unlock.password.submit')}
      </Button>
    </form>
  )
}
