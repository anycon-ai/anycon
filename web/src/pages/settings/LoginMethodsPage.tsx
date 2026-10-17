import { useState } from 'react'
import { Plus, Trash2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useCreateToken, useDeleteToken, useTokens } from '@/api/queries'
import { describeError } from '@/lib/errors'
import { formatDateTime, formatRelative } from '@/lib/format'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'
import { Card, CopyButton, SectionTitle, Skeleton } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

/** v0.1 只有 token 登录；passkey / OAuth 在后续版本。 */
export function LoginMethodsPage() {
  const { t, i18n } = useTranslation()
  const tokens = useTokens()
  const create = useCreateToken()
  const del = useDeleteToken()
  const [open, setOpen] = useState(false)
  const [label, setLabel] = useState('')
  const [created, setCreated] = useState<string | null>(null)
  const last = (tokens.data?.length ?? 0) <= 1

  return (
    <div className="space-y-6">
      <h1 className="text-xl font-semibold">{t('settings.login')}</h1>
      <section>
        <SectionTitle
          action={
            <Button size="sm" variant="primary" icon={<Plus className="size-4" />} onClick={() => { setLabel(''); setCreated(null); setOpen(true) }} data-testid="token-new">
              {t('settings.tokenNew')}
            </Button>
          }
        >
          {t('settings.tokens')}
        </SectionTitle>
        <p className="mb-3 text-sm text-fg-muted">{t('settings.tokensHint')}</p>
        {tokens.isLoading ? <Skeleton className="h-14" /> : null}
        <ul className="space-y-2" data-testid="token-list">
          {(tokens.data ?? []).map((tok) => (
            <li key={tok.id}>
              <Card className="flex items-center gap-3 p-3">
                <div className="min-w-0 flex-1">
                  <p className="truncate font-medium">{tok.label || tok.id}</p>
                  <p className="text-xs text-fg-subtle">
                    {t('settings.tokenCreated', { when: formatDateTime(tok.created_at_ms, i18n.language) })}
                    {tok.last_used_ms ? ` · ${t('settings.tokenUsed', { when: formatRelative(tok.last_used_ms, i18n.language) })}` : ` · ${t('settings.tokenNeverUsed')}`}
                  </p>
                </div>
                <Button
                  size="sm"
                  variant="outline-danger"
                  icon={<Trash2 className="size-4" />}
                  disabled={last}
                  title={last ? t('settings.lastMethod') : undefined}
                  onClick={() => del.mutate(tok.id, { onError: (e) => toast.error(describeError(e, t)) })}
                  aria-label={t('settings.tokenDelete')}
                />
              </Card>
            </li>
          ))}
        </ul>
        {last && (tokens.data?.length ?? 0) === 1 ? <p className="mt-2 text-xs text-fg-subtle">{t('settings.lastMethod')}</p> : null}
      </section>

      <Dialog open={open} onOpenChange={setOpen} title={created ? t('settings.tokenCreatedTitle') : t('settings.tokenNew')} closeLabel={t('common.close')}>
        {created ? (
          <div className="space-y-3" data-testid="token-created">
            <p className="text-sm text-warning">{t('settings.tokenOnce')}</p>
            <code className="block break-all rounded-md bg-bg-muted px-3 py-2 font-mono text-sm" data-testid="token-value">
              {created}
            </code>
            <div className="flex justify-between">
              <CopyButton text={created} />
              <Button variant="primary" onClick={() => setOpen(false)}>
                {t('common.done')}
              </Button>
            </div>
          </div>
        ) : (
          <form
            className="space-y-4"
            onSubmit={(e) => {
              e.preventDefault()
              create.mutate(label.trim() || 'token', {
                onSuccess: (r) => setCreated(r.token),
                onError: (err) => toast.error(describeError(err, t)),
              })
            }}
          >
            <Field label={t('settings.tokenLabel')} htmlFor="token-label">
              <Input id="token-label" value={label} onChange={(e) => setLabel(e.target.value)} placeholder={t('settings.tokenLabelPlaceholder')} autoFocus maxLength={80} />
            </Field>
            <div className="flex justify-end">
              <Button type="submit" variant="primary" loading={create.isPending}>
                {t('settings.tokenCreate')}
              </Button>
            </div>
          </form>
        )}
      </Dialog>
    </div>
  )
}
