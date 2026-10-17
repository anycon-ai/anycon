import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react'
import { KeyRound, ShieldAlert, ShieldCheck } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { AuditEntry, NodeAuditResult, NodeEnrollStatusResult, NodeSetEnrollEnabledParams, NodeSetEnrollPasswordParams } from '@/protocol'
import { EnrollRequestCard } from '@/features/approve/EnrollRequestCard'
import { refreshEnrolls } from '@/features/approve/actions'
import { describeError } from '@/lib/errors'
import { formatCountdown } from '@/lib/expiry'
import { formatRelative } from '@/lib/format'
import { cleanHintText } from '@/lib/hint'
import { MIN_PASSWORD_CHARS, passwordLongEnough } from '@/lib/password'
import { useNow } from '@/lib/useNow'
import { rpc } from '@/transport/instance'
import { isPendingApproval, isPendingScan, useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'
import { Card, Chip, SectionTitle, Skeleton, Switch, type Tone } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

const STATE_TONE: Record<'unset' | 'enabled' | 'disabled' | 'locked', Tone> = { unset: 'neutral', enabled: 'success', disabled: 'warning', locked: 'danger' }

function useEnrollStatus(nodeId: string) {
  const [status, setStatus] = useState<NodeEnrollStatusResult | null>(null)
  const [fetchedAt, setFetchedAt] = useState(0)
  const [failed, setFailed] = useState(false)
  const tick = useEnroll((s) => s.securityTick[nodeId] ?? 0)
  const online = useNodes((s) => s.byId[nodeId]?.conn === 'ready')
  const load = useCallback(async () => {
    try {
      const res = await rpc().request<NodeEnrollStatusResult>(nodeId, 'node.enroll_status', {})
      setStatus(res)
      setFetchedAt(Date.now())
      setFailed(false)
    } catch (e) {
      console.warn('anycon: could not read the enrollment status', e)
      setFailed(true)
    }
  }, [nodeId])
  // 打开标签、节点上线、别的设备改了口令 / 设置（node.security_event）时重新读
  useEffect(() => {
    if (online) void load()
  }, [online, load, tick])
  return { status, fetchedAt, failed, reload: load, online }
}

/**
 * 节点设置的「接入」标签（ADR-36，spec/web.md § 4.8）：节点口令（设置 / 修改 / 清除 / 启用停用 / 锁定后重新启用）、
 * 批准接入开关、待批准的接入请求、最近的接入活动。
 */
export function EnrollTab({ nodeId, autoReview }: { nodeId: string; autoReview?: boolean }) {
  const { t } = useTranslation()
  const { status, fetchedAt, failed, reload, online } = useEnrollStatus(nodeId)
  const items = useEnroll((s) => s.items)
  const resolved = useEnroll((s) => s.resolved)
  const dialog = useEnroll((s) => s.dialog)
  const now = useNow(1000, true)

  // 打开标签时补读进行中的请求（实时的靠 node.enroll_requested / enroll_resolved）
  useEffect(() => {
    if (online) void refreshEnrolls(nodeId)
  }, [online, nodeId])

  const pending = Object.values(items)
    .filter((it) => it.nodeId === nodeId && (isPendingApproval(it, resolved, now) || isPendingScan(it, resolved, now)))
    .sort((a, b) => a.receivedAt - b.receivedAt)

  // 从推送点进来：自动弹出第一个待批准请求的对话框（只弹一次）
  const reviewed = useRef(false)
  useEffect(() => {
    const first = pending.find((p) => p.info.method === 'code')
    if (autoReview && !reviewed.current && first && !dialog) {
      reviewed.current = true
      useEnroll.getState().openDialog(nodeId, first.info.enroll_id)
    }
  }, [autoReview, pending, dialog, nodeId])

  return (
    <div className="space-y-6" data-testid="enroll-tab">
      <p className="text-sm text-fg-muted">{t('enrollTab.intro')}</p>
      {failed && !status ? <p className="text-sm text-danger">{t('enrollTab.loadFailed')}</p> : null}
      <PasswordSection nodeId={nodeId} status={status} fetchedAt={fetchedAt} online={online} reload={reload} />
      <ApprovalSection nodeId={nodeId} status={status} online={online} reload={reload} />
      <section>
        <SectionTitle>{t('enrollTab.pendingTitle')}</SectionTitle>
        {pending.length === 0 ? (
          <p className="text-sm text-fg-subtle" data-testid="enroll-pending-empty">
            {t('enrollTab.pendingEmpty')}
          </p>
        ) : (
          <ul className="space-y-2" data-testid="enroll-pending">
            {pending.map((it) => (
              <li key={it.info.enroll_id}>
                <Card className="p-3">
                  <EnrollRequestCard nodeId={nodeId} enrollId={it.info.enroll_id} onScan={() => useEnroll.getState().openScanner()} />
                </Card>
              </li>
            ))}
          </ul>
        )}
      </section>
      <ActivitySection nodeId={nodeId} online={online} />
    </div>
  )
}

// ---- 节点口令 ----

function PasswordSection({ nodeId, status, fetchedAt, online, reload }: { nodeId: string; status: NodeEnrollStatusResult | null; fetchedAt: number; online: boolean; reload: () => Promise<void> }) {
  const { t, i18n } = useTranslation()
  const [setOpen, setSetOpen] = useState(false)
  const [clearOpen, setClearOpen] = useState(false)
  const [reenableOpen, setReenableOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const devices = useNodes((s) => s.byId[nodeId]?.devices)
  const self = useHub((s) => s.snapshot.deviceId)
  const now = useNow(1000, !!status?.password.retry_after_ms)

  const pw = status?.password
  const state: 'unset' | 'enabled' | 'disabled' | 'locked' = !pw || !pw.set ? 'unset' : pw.state
  const setterName = (id: string | null | undefined): string => {
    if (!id) return t('security.someone')
    if (id === 'local') return t('security.local')
    if (id === self) return t('approve.thisDevice')
    return cleanHintText(devices?.find((d) => d.device_id === id)?.name) || id.slice(0, 8)
  }

  const call = async (method: 'node.set_enroll_password' | 'node.set_enroll_enabled', params: NodeSetEnrollPasswordParams | NodeSetEnrollEnabledParams, ok: string): Promise<boolean> => {
    setBusy(true)
    try {
      await rpc().request(nodeId, method, params)
      toast.success(ok)
      await reload()
      return true
    } catch (e) {
      toast.error(describeError(e, t))
      return false
    } finally {
      setBusy(false)
    }
  }

  const retryLeft = pw?.retry_after_ms ? pw.retry_after_ms - (now - fetchedAt) : 0
  return (
    <section data-testid="enroll-password" data-state={state}>
      <SectionTitle>{t('enrollTab.passwordTitle')}</SectionTitle>
      <Card className="space-y-3 p-4" tone={state === 'locked' ? 'danger' : undefined}>
        <div className="flex flex-wrap items-center gap-2">
          <KeyRound className="size-5 text-fg-subtle" aria-hidden />
          <Chip tone={STATE_TONE[state]} testId="enroll-password-state">
            {t(`enrollTab.state.${state}`)}
          </Chip>
          {pw && pw.failures > 0 ? <Chip tone="warning">{t('enrollTab.failures', { count: pw.failures })}</Chip> : null}
        </div>
        <p className="text-sm text-fg-muted">{state === 'unset' ? t('enrollTab.passwordUnset') : t('enrollTab.passwordHint')}</p>
        {pw?.set && pw.set_at_ms ? (
          <p className="text-xs text-fg-subtle" data-testid="enroll-password-setter">
            {t('enrollTab.setAt', { when: formatRelative(pw.set_at_ms, i18n.language), who: setterName(pw.set_by) })}
          </p>
        ) : null}
        {retryLeft > 0 ? (
          <p className="text-xs text-warning" data-testid="enroll-password-backoff">
            {t('enrollTab.backoff', { time: formatCountdown(retryLeft) })}
          </p>
        ) : null}
        {state === 'locked' ? (
          <div className="space-y-2 rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm text-danger" role="alert" data-testid="enroll-locked">
            <p className="flex items-start gap-2">
              <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
              {t('enrollTab.lockedBody')}
            </p>
            <Button size="sm" variant="outline-danger" disabled={!online} onClick={() => setReenableOpen(true)} data-testid="enroll-reenable">
              {t('enrollTab.reenable')}
            </Button>
          </div>
        ) : null}
        <div className="flex flex-wrap items-center gap-2">
          <Button variant="primary" size="sm" disabled={!online || busy} onClick={() => setSetOpen(true)} data-testid="enroll-password-set">
            {state === 'unset' ? t('enrollTab.setPassword') : t('enrollTab.changePassword')}
          </Button>
          {state !== 'unset' ? (
            <Button variant="outline-danger" size="sm" disabled={!online || busy} onClick={() => setClearOpen(true)} data-testid="enroll-password-clear">
              {t('enrollTab.clearPassword')}
            </Button>
          ) : null}
          {state === 'enabled' || state === 'disabled' ? (
            <label className="ml-auto flex items-center gap-2 text-sm">
              <Switch checked={state === 'enabled'} disabled={!online || busy} label={t('enrollTab.passwordEnabled')} onCheckedChange={(v) => void call('node.set_enroll_enabled', { password: v }, v ? t('enrollTab.enabledToast') : t('enrollTab.disabledToast'))} />
              {t('enrollTab.passwordEnabled')}
            </label>
          ) : null}
        </div>
      </Card>

      <SetPasswordDialog
        open={setOpen}
        onOpenChange={setSetOpen}
        replacing={state !== 'unset'}
        busy={busy}
        onSubmit={async (password) => {
          if (await call('node.set_enroll_password', { password }, t('enrollTab.passwordSaved'))) setSetOpen(false)
        }}
      />
      <Dialog open={clearOpen} onOpenChange={setClearOpen} title={t('enrollTab.clearTitle')} description={t('enrollTab.clearBody')} closeLabel={t('common.close')} testId="enroll-clear-dialog">
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setClearOpen(false)}>
            {t('common.cancel')}
          </Button>
          <Button
            variant="danger"
            loading={busy}
            data-testid="enroll-clear-confirm"
            onClick={() =>
              void call('node.set_enroll_password', {}, t('enrollTab.passwordCleared')).then((ok) => {
                if (ok) setClearOpen(false)
              })
            }
          >
            {t('enrollTab.clearPassword')}
          </Button>
        </div>
      </Dialog>
      <Dialog open={reenableOpen} onOpenChange={setReenableOpen} title={t('enrollTab.reenableTitle')} description={t('enrollTab.reenableBody')} closeLabel={t('common.close')} testId="enroll-reenable-dialog">
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setReenableOpen(false)}>
            {t('common.cancel')}
          </Button>
          <Button
            variant="danger"
            loading={busy}
            data-testid="enroll-reenable-confirm"
            onClick={() =>
              void call('node.set_enroll_enabled', { password: true }, t('enrollTab.enabledToast')).then((ok) => {
                if (ok) setReenableOpen(false)
              })
            }
          >
            {t('enrollTab.reenable')}
          </Button>
        </div>
      </Dialog>
    </section>
  )
}

/** 设置 / 修改口令：输两次、至少 8 个字符。口令只在这个对话框的状态里，提交后立刻清掉。 */
export function SetPasswordDialog({ open, onOpenChange, replacing, busy, onSubmit }: { open: boolean; onOpenChange: (o: boolean) => void; replacing: boolean; busy: boolean; onSubmit: (password: string) => Promise<void> }) {
  const { t } = useTranslation()
  const [first, setFirst] = useState('')
  const [second, setSecond] = useState('')
  const [touched, setTouched] = useState(false)
  const reset = (): void => {
    setFirst('')
    setSecond('')
    setTouched(false)
  }
  const close = (): void => {
    reset()
    onOpenChange(false)
  }
  const tooShort = !passwordLongEnough(first)
  const mismatch = first !== second
  const submit = (e: FormEvent): void => {
    e.preventDefault()
    setTouched(true)
    if (tooShort || mismatch) return
    const password = first
    reset() // 口令用完就丢
    void onSubmit(password)
  }
  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (!o) reset()
        onOpenChange(o)
      }}
      title={replacing ? t('enrollTab.changeTitle') : t('enrollTab.setTitle')}
      description={t('enrollTab.setBody')}
      closeLabel={t('common.close')}
      testId="enroll-password-dialog"
    >
      <form className="space-y-3" onSubmit={submit}>
        <Field label={t('enrollTab.newPassword')} htmlFor="enroll-pw-1" hint={t('enrollTab.passwordRule', { count: MIN_PASSWORD_CHARS })} error={touched && tooShort ? t('enrollTab.tooShort', { count: MIN_PASSWORD_CHARS }) : undefined}>
          <Input id="enroll-pw-1" data-testid="enroll-pw-1" type="password" autoComplete="new-password" value={first} onChange={(e) => setFirst(e.target.value)} />
        </Field>
        <Field label={t('enrollTab.repeatPassword')} htmlFor="enroll-pw-2" error={touched && !tooShort && mismatch ? t('enrollTab.mismatch') : undefined}>
          <Input id="enroll-pw-2" data-testid="enroll-pw-2" type="password" autoComplete="new-password" value={second} onChange={(e) => setSecond(e.target.value)} />
        </Field>
        <div className="flex justify-end gap-2 pt-1">
          <Button variant="outline" onClick={close}>
            {t('common.cancel')}
          </Button>
          <Button type="submit" variant="primary" loading={busy} data-testid="enroll-pw-submit">
            {t('enrollTab.savePassword')}
          </Button>
        </div>
      </form>
    </Dialog>
  )
}

// ---- 批准接入 ----

function ApprovalSection({ nodeId, status, online, reload }: { nodeId: string; status: NodeEnrollStatusResult | null; online: boolean; reload: () => Promise<void> }) {
  const { t } = useTranslation()
  const [busy, setBusy] = useState(false)
  const change = async (approval: boolean): Promise<void> => {
    setBusy(true)
    try {
      const params: NodeSetEnrollEnabledParams = { approval }
      await rpc().request(nodeId, 'node.set_enroll_enabled', params)
      toast.success(approval ? t('enrollTab.approvalOnToast') : t('enrollTab.approvalOffToast'))
      await reload()
    } catch (e) {
      toast.error(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }
  return (
    <section data-testid="enroll-approval">
      <SectionTitle>{t('enrollTab.approvalTitle')}</SectionTitle>
      <Card className="flex items-start gap-3 p-4">
        <ShieldCheck className="mt-0.5 size-5 shrink-0 text-fg-subtle" aria-hidden />
        <div className="min-w-0 flex-1">
          <p className="text-sm font-medium">{t('enrollTab.approvalLabel')}</p>
          <p className="text-sm text-fg-muted">{t('enrollTab.approvalHint')}</p>
        </div>
        {status ? <Switch checked={status.approval} disabled={!online || busy} label={t('enrollTab.approvalLabel')} onCheckedChange={(v) => void change(v)} /> : <Skeleton className="h-5 w-9" />}
      </Card>
    </section>
  )
}

// ---- 最近的接入活动 ----

const ACTIVITY_LIMIT = 20

/** `node.audit` 里的 `enroll.*`，新的在前，最多 20 条。 */
export function recentEnrollActivity(entries: readonly AuditEntry[], limit = ACTIVITY_LIMIT): AuditEntry[] {
  return entries
    .filter((e) => e.action.startsWith('enroll.'))
    .sort((a, b) => b.ts_ms - a.ts_ms)
    .slice(0, limit)
}

const KNOWN_ACTIONS = new Set(['requested', 'approved', 'denied', 'code', 'completed', 'failed', 'password_set', 'password_cleared', 'password_locked', 'settings'])

function detailText(detail: unknown): string {
  if (typeof detail !== 'object' || detail === null) return ''
  const d = detail as Record<string, unknown>
  const parts: string[] = []
  for (const k of ['method', 'result', 'reason']) if (typeof d[k] === 'string') parts.push(cleanHintText(d[k], 32))
  if (typeof d.failures === 'number') parts.push(`#${d.failures}`)
  return parts.filter(Boolean).join(' · ')
}

function ActivitySection({ nodeId, online }: { nodeId: string; online: boolean }) {
  const { t, i18n } = useTranslation()
  const [entries, setEntries] = useState<AuditEntry[] | null>(null)
  const [failed, setFailed] = useState(false)
  const tick = useEnroll((s) => s.securityTick[nodeId] ?? 0)
  useEffect(() => {
    if (!online) return
    let alive = true
    rpc()
      .request<NodeAuditResult>(nodeId, 'node.audit', { limit: 200 })
      .then((res) => alive && (setEntries(recentEnrollActivity(res.entries)), setFailed(false)))
      .catch((e: unknown) => {
        console.warn('anycon: could not read the audit log', e)
        if (alive) setFailed(true)
      })
    return () => {
      alive = false
    }
  }, [nodeId, online, tick])
  return (
    <section data-testid="enroll-activity">
      <SectionTitle>{t('enrollTab.activityTitle')}</SectionTitle>
      {failed && !entries ? <p className="text-sm text-danger">{t('enrollTab.activityFailed')}</p> : null}
      {entries === null ? (
        failed ? null : <Skeleton className="h-16" />
      ) : entries.length === 0 ? (
        <p className="text-sm text-fg-subtle">{t('enrollTab.activityEmpty')}</p>
      ) : (
        <ul className="divide-y divide-border overflow-hidden rounded-lg border border-border bg-surface text-sm" data-testid="enroll-activity-list">
          {entries.map((e, i) => {
            const suffix = e.action.slice('enroll.'.length)
            const label = KNOWN_ACTIONS.has(suffix) ? t(`enrollTab.audit.${suffix}`) : cleanHintText(e.action, 48)
            const detail = detailText(e.detail)
            return (
              <li key={`${e.ts_ms}-${i}`} className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 px-3 py-2" data-action={e.action}>
                <span className="font-medium">{label}</span>
                <span className="min-w-0 flex-1 truncate text-fg-muted">
                  {[cleanHintText(e.device_name), detail].filter(Boolean).join(' · ')}
                </span>
                <span className="text-xs text-fg-subtle">{formatRelative(e.ts_ms, i18n.language)}</span>
              </li>
            )
          })}
        </ul>
      )}
    </section>
  )
}
