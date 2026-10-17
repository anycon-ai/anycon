import { useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { promoteSession } from '@/features/session/actions'
import { describeError } from '@/lib/errors'
import { refreshProjects } from '@/features/sync/notifications'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'

/** scratch 会话转成项目。 */
export function PromoteDialog({ nodeId, sessionId, open, onOpenChange }: { nodeId: string; sessionId: string; open: boolean; onOpenChange: (v: boolean) => void }) {
  const { t } = useTranslation()
  const [name, setName] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const submit = async (e: FormEvent): Promise<void> => {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      await promoteSession(nodeId, sessionId, name.trim())
      await refreshProjects(nodeId)
      onOpenChange(false)
    } catch (err) {
      setError(describeError(err, t))
    } finally {
      setBusy(false)
    }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={t('session.promoteTitle')} description={t('session.promoteBody')} closeLabel={t('common.close')}>
      <form onSubmit={(e) => void submit(e)} className="space-y-4">
        <Field label={t('project.name')} htmlFor="promote-name" error={error}>
          <Input id="promote-name" value={name} onChange={(e) => setName(e.target.value)} autoFocus placeholder="my-app" autoCapitalize="off" spellCheck={false} />
        </Field>
        <div className="flex justify-end">
          <Button type="submit" variant="primary" loading={busy} disabled={!name.trim()}>
            {t('session.promote')}
          </Button>
        </div>
      </form>
    </Dialog>
  )
}
