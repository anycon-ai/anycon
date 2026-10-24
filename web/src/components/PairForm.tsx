import { useState, type FormEvent } from 'react'
import { Check, Circle, Link2, Pencil, ShieldAlert } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { QuotaLink } from '@/components/QuotaLink'
import { usePairing, type PairPhase } from '@/features/pairing/usePairing'
import { extractPairCode, formatPairCodeInput, isCompletePairCode } from '@/lib/code'
import { useHub } from '@/stores/hub'
import { getClient } from '@/transport/instance'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Field, Input } from '@/ui/Field'
import { Spinner } from '@/ui/Misc'

const STEPS: Array<{ key: Exclude<PairPhase, 'idle' | 'error' | 'done' | 'connecting'>; label: string }> = [
  // 派生密钥是一次 Argon2id（64 MiB × 3 轮），浏览器里 0.1–1 s，肉眼可感，所以单独一步
  { key: 'deriving', label: 'pair.steps.deriving' },
  { key: 'matching', label: 'pair.steps.matching' },
  { key: 'handshaking', label: 'pair.steps.handshaking' },
  { key: 'confirming', label: 'pair.steps.confirming' },
  { key: 'verifying', label: 'pair.steps.verifying' },
]
const ORDER: PairPhase[] = ['idle', 'connecting', 'deriving', 'matching', 'handshaking', 'confirming', 'verifying', 'done']

/**
 * 短码输入 + 进度 + 错误分支。`/pair` 和 `/setup` 共用。
 * `fromLink`：短码是从配对链接带进来的（`#code=`）。这种情况不自动开始，先显示确认步骤——
 * 否则别人的 daemon 生成的链接被已登录的用户点开，那台 daemon 就会被静默加进这个账号（docs/spec/web.md § 4.3）。
 */
export function PairForm({ initialCode, fromLink, onPaired }: { initialCode?: string; fromLink?: boolean; onPaired: (nodeId: string) => void }) {
  const { t } = useTranslation()
  const [code, setCode] = useState(() => formatPairCodeInput(initialCode ?? ''))
  const [needsConfirm, setNeedsConfirm] = useState(() => !!fromLink && isCompletePairCode(initialCode ?? ''))
  const hubName = useHub((s) => s.snapshot.deviceName)
  const [name, setName] = useState('')
  const [editingName, setEditingName] = useState(false)
  const { state, start } = usePairing()
  const deviceName = name || hubName || getClient().deviceName
  const busy = state.phase !== 'idle' && state.phase !== 'error' && state.phase !== 'done'

  const run = async (c: string): Promise<void> => {
    const id = await start(c, deviceName)
    if (id) onPaired(id)
  }

  const submit = (e: FormEvent): void => {
    e.preventDefault()
    void run(code)
  }

  if (needsConfirm && state.phase === 'idle')
    return (
      <div className="space-y-4" data-testid="pair-link-confirm">
        <div className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning-subtle p-3 text-sm text-warning">
          <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
          <p>{t('pair.link.warning')}</p>
        </div>
        <p className="text-center font-mono text-lg tracking-[0.15em]" data-testid="pair-link-code">
          {code}
        </p>
        <p className="text-sm text-fg-muted">{t('pair.deviceAs', { name: deviceName })}</p>
        <div className="flex flex-col gap-2">
          <Button
            variant="primary"
            size="lg"
            icon={<Link2 className="size-4" />}
            onClick={() => {
              setNeedsConfirm(false)
              void run(code)
            }}
            data-testid="pair-link-confirm-submit"
          >
            {t('pair.link.confirm')}
          </Button>
          <Button
            variant="ghost"
            onClick={() => {
              setCode('')
              setNeedsConfirm(false)
            }}
            data-testid="pair-link-cancel"
          >
            {t('pair.link.cancel')}
          </Button>
        </div>
      </div>
    )

  const long = code.length > 9
  const errKey = state.error ? (state.error === 'timeout' || state.error === 'handshake_failed' || state.error === 'peer_gone' ? 'pair.errors.retry' : `pair.errors.${state.error}`) : null

  return (
    <form onSubmit={submit} className="space-y-4" aria-busy={busy}>
      <Field label={t('pair.codeLabel')} htmlFor="pair-code" hint={t('pair.codeHint')} error={errKey ? <PairErrorText errKey={errKey} quota={state.error === 'quota'} /> : undefined}>
        <Input
          id="pair-code"
          data-testid="pair-code-input"
          value={code}
          onChange={(e) => setCode(extractPairCode(e.target.value))}
          onPaste={(e) => {
            // 整条配对链接（…/pair#code=XXXX-XXXX）粘贴进来也认，取其中的短码
            e.preventDefault()
            setCode(extractPairCode(e.clipboardData.getData('text')))
          }}
          autoComplete="off"
          autoCapitalize="characters"
          autoCorrect="off"
          spellCheck={false}
          inputMode="text"
          maxLength={19}
          disabled={busy}
          placeholder="XXXX-XXXX"
          className={cn('h-12 text-center font-mono', long ? 'text-base tracking-[0.12em]' : 'text-xl tracking-[0.25em]')}
          aria-invalid={state.error ? true : undefined}
        />
      </Field>

      <div className="text-sm">
        {editingName ? (
          <Field label={t('pair.deviceName')} htmlFor="pair-device-name">
            <Input id="pair-device-name" data-testid="pair-device-name" value={name || deviceName} onChange={(e) => setName(e.target.value)} disabled={busy} maxLength={80} />
          </Field>
        ) : (
          <p className="flex items-center gap-2 text-fg-muted">
            <span>{t('pair.deviceAs', { name: deviceName })}</span>
            <button type="button" className="inline-flex items-center gap-1 text-accent-text hover:underline" onClick={() => setEditingName(true)} disabled={busy}>
              <Pencil className="size-3" />
              {t('pair.rename')}
            </button>
          </p>
        )}
      </div>

      {busy || state.phase === 'done' ? (
        <ol className="space-y-2 rounded-md border border-border bg-bg-subtle p-3 text-sm" data-testid="pair-progress" data-phase={state.phase}>
          <li className="flex items-center gap-2">
            <StepIcon status={stepStatus(state.phase, 'connecting')} />
            <span>{t('pair.steps.connecting')}</span>
          </li>
          {STEPS.map((s) => (
            <li key={s.key} className="flex items-center gap-2">
              <StepIcon status={stepStatus(state.phase, s.key)} />
              <span className={cn(stepStatus(state.phase, s.key) === 'todo' && 'text-fg-subtle')}>{t(s.label)}</span>
            </li>
          ))}
        </ol>
      ) : null}

      <Button type="submit" variant="primary" size="lg" className="w-full" loading={busy} disabled={!isCompletePairCode(code)} icon={<Link2 className="size-4" />} data-testid="pair-submit">
        {t('pair.submit')}
      </Button>
    </form>
  )
}

function stepStatus(phase: PairPhase, step: PairPhase): 'done' | 'active' | 'todo' {
  const p = ORDER.indexOf(phase)
  const s = ORDER.indexOf(step)
  if (phase === 'done' || p > s) return 'done'
  return p === s ? 'active' : 'todo'
}

function StepIcon({ status }: { status: 'done' | 'active' | 'todo' }) {
  if (status === 'done') return <Check className="size-4 text-success" aria-hidden />
  if (status === 'active') return <Spinner className="text-accent-text" />
  return <Circle className="size-4 text-fg-subtle" aria-hidden />
}

/** 配对失败的说明；配额用完（`pair_failed { quota }`）时带一个去「配额」页的链接。 */
function PairErrorText({ errKey, quota }: { errKey: string; quota: boolean }) {
  const { t } = useTranslation()
  return (
    <>
      {t(errKey, { defaultValue: t('pair.errors.generic') })}
      {quota ? (
        <>
          {' '}
          <QuotaLink />
        </>
      ) : null}
    </>
  )
}
