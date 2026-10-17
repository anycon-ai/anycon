import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useConfig, useMe, usePatchMe } from '@/api/queries'
import { EmailSection } from '@/features/auth/EmailSection'
import { describeError } from '@/lib/errors'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'
import { Card, Chip } from '@/ui/Misc'

export function ProfilePage() {
  const { t } = useTranslation()
  const me = useMe()
  const patch = usePatchMe()
  const config = useConfig()
  // 配了 SMTP 才能绑定 / 用邮箱登录（ADR-37）；没配就只读显示账号上已有的邮箱
  const emailEnabled = config.data?.login_methods.includes('email') ?? false
  const [name, setName] = useState('')
  useEffect(() => {
    if (me.data) setName(me.data.display_name)
  }, [me.data])
  if (!me.data) return null
  return (
    <div className="space-y-6">
      <h1 className="text-xl font-semibold">{t('settings.profile')}</h1>
      <Card className="p-4">
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault()
            patch.mutate(name.trim(), {
              onSuccess: () => toast.success(t('settings.saved')),
              onError: (err) => toast.error(describeError(err, t)),
            })
          }}
        >
          <Field label={t('settings.displayName')} htmlFor="display-name">
            <Input id="display-name" value={name} onChange={(e) => setName(e.target.value)} maxLength={80} data-testid="display-name" />
          </Field>
          <div className="flex items-center gap-2">
            {me.data.is_admin ? <Chip tone="accent">{t('settings.admin')}</Chip> : null}
          </div>
          <Button type="submit" variant="primary" loading={patch.isPending} disabled={!name.trim() || name.trim() === me.data.display_name}>
            {t('common.save')}
          </Button>
        </form>
      </Card>
      <Card className="p-4">
        {emailEnabled ? (
          <EmailSection />
        ) : (
          <Field label={t('settings.email')} hint={me.data.email ? undefined : t('settings.noEmail')}>
            <p className="text-sm">{me.data.email ?? '—'}</p>
          </Field>
        )}
      </Card>
    </div>
  )
}
