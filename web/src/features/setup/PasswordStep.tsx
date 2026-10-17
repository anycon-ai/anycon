import { useState, type FormEvent } from 'react'
import { KeyRound } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { NodeSetEnrollPasswordParams } from '@/protocol'
import { describeError } from '@/lib/errors'
import { MIN_PASSWORD_CHARS, passwordLongEnough } from '@/lib/password'
import { rpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'

/**
 * `/setup` 配对完成后：问要不要给这台节点设口令（ADR-36）。口令让用户出门在外、手边没有已配对设备时，
 * 在任何设备上解锁这台节点。可以跳过；之后在节点设置的「接入」标签里设。口令只在这个组件的状态里，提交后立刻清掉。
 */
export function PasswordStep({ nodeId, onDone }: { nodeId: string; onDone: () => void }) {
  const { t } = useTranslation()
  const ready = useNodes((s) => s.byId[nodeId]?.conn === 'ready')
  const [first, setFirst] = useState('')
  const [second, setSecond] = useState('')
  const [touched, setTouched] = useState(false)
  const [busy, setBusy] = useState(false)
  const tooShort = !passwordLongEnough(first)
  const mismatch = first !== second

  const submit = async (e: FormEvent): Promise<void> => {
    e.preventDefault()
    setTouched(true)
    if (tooShort || mismatch || !ready) return
    const password = first
    setFirst('')
    setSecond('')
    setBusy(true)
    try {
      const params: NodeSetEnrollPasswordParams = { password }
      await rpc().request(nodeId, 'node.set_enroll_password', params)
      toast.success(t('enrollTab.passwordSaved'))
      onDone()
    } catch (err) {
      toast.error(describeError(err, t))
    } finally {
      setBusy(false)
    }
  }

  return (
    <form className="space-y-4 text-left" onSubmit={(e) => void submit(e)} data-testid="setup-password">
      <div className="flex items-center gap-2">
        <KeyRound className="size-5 text-accent-text" aria-hidden />
        <h1 className="text-lg font-semibold">{t('setup.passwordTitle')}</h1>
      </div>
      <p className="text-sm text-fg-muted">{t('setup.passwordBody')}</p>
      {!ready ? <p className="text-xs text-fg-subtle">{t('setup.passwordConnecting')}</p> : null}
      <Field label={t('enrollTab.newPassword')} htmlFor="setup-pw-1" hint={t('enrollTab.passwordRule', { count: MIN_PASSWORD_CHARS })} error={touched && tooShort ? t('enrollTab.tooShort', { count: MIN_PASSWORD_CHARS }) : undefined}>
        <Input id="setup-pw-1" data-testid="setup-pw-1" type="password" autoComplete="new-password" value={first} onChange={(e) => setFirst(e.target.value)} />
      </Field>
      <Field label={t('enrollTab.repeatPassword')} htmlFor="setup-pw-2" error={touched && !tooShort && mismatch ? t('enrollTab.mismatch') : undefined}>
        <Input id="setup-pw-2" data-testid="setup-pw-2" type="password" autoComplete="new-password" value={second} onChange={(e) => setSecond(e.target.value)} />
      </Field>
      <div className="flex justify-between gap-2">
        <Button variant="ghost" onClick={onDone} disabled={busy} data-testid="setup-pw-skip">
          {t('setup.skip')}
        </Button>
        <Button type="submit" variant="primary" loading={busy} disabled={!ready} data-testid="setup-pw-submit">
          {t('enrollTab.savePassword')}
        </Button>
      </div>
    </form>
  )
}
