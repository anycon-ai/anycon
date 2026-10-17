// 每个通知类别的 push / email 开关（`GET` / `PUT /api/me/notifications`，spec/hub.md § 6.3）。
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { useConfig, useMe } from '@/api/queries'
import { useNotificationPrefs, useSaveNotificationPrefs } from '@/api/notifications'
import type { ApiNotificationPref } from '@/api/types'
import { describeError } from '@/lib/errors'
import { toast } from '@/stores/ui'
import { Card, Skeleton, Switch } from '@/ui/Misc'
import { forgetSystemNotifyPrefs } from './prefsCache'
import type { NotifyCategory } from '@/protocol'

/** 界面上的顺序：要人马上处理的在前。 */
export const CATEGORY_ORDER: readonly NotifyCategory[] = ['permission_requested', 'error', 'enroll_requested', 'security', 'turn_finished', 'session_idle']
/** 能发邮件的四个类别（ADR-37）；另外两个只有推送。 */
export const EMAIL_CATEGORIES: ReadonlySet<NotifyCategory> = new Set<NotifyCategory>(['permission_requested', 'error', 'enroll_requested', 'security'])

/** hub 没回的类别按「全关」补上，界面总是六行。 */
export function withAllCategories(rows: readonly ApiNotificationPref[]): ApiNotificationPref[] {
  return CATEGORY_ORDER.map((category) => rows.find((r) => r.category === category) ?? { category, push: false, email: false })
}

export function CategoriesCard() {
  const { t } = useTranslation()
  const prefs = useNotificationPrefs()
  const config = useConfig()
  const me = useMe()
  const save = useSaveNotificationPrefs(forgetSystemNotifyPrefs)

  if (prefs.isLoading) return <Skeleton className="h-64" />
  if (!prefs.data) {
    return (
      <p role="alert" className="text-sm text-danger" data-testid="notify-load-failed">
        {t('notifications.categories.loadFailed')}
      </p>
    )
  }

  const rows = withAllCategories(prefs.data.categories)
  const emailAvailable = prefs.data.email_available
  // 邮件那一列不能用的原因：hub 没配 SMTP（登录方式里没有 email），还是账号没绑邮箱
  const smtp = config.data?.login_methods.includes('email') ?? false
  const emailWhy: 'smtp' | 'bind' | null = emailAvailable ? null : !smtp ? 'smtp' : !me.data?.email ? 'bind' : null

  const toggle = (category: NotifyCategory, field: 'push' | 'email', value: boolean): void => {
    const next = rows.map((r) => (r.category === category ? { ...r, [field]: value } : r))
    save.mutate(next, { onError: (e) => toast.error(t('notifications.categories.saveFailed', { error: describeError(e, t) })) })
  }

  return (
    <section className="space-y-3" data-testid="categories-card">
      <div>
        <h2 className="text-sm font-semibold">{t('notifications.categories.title')}</h2>
        <p className="mt-1 text-sm text-fg-muted">{t('notifications.categories.hint')}</p>
      </div>
      {emailWhy ? (
        <p className="text-sm text-fg-muted" data-testid="notify-email-why" data-why={emailWhy}>
          {emailWhy === 'smtp' ? (
            t('notifications.categories.emailNoSmtp')
          ) : (
            <>
              {t('notifications.categories.emailNoAddress')}{' '}
              <Link to="/settings" className="text-accent-text underline">
                {t('settings.profile')}
              </Link>
            </>
          )}
        </p>
      ) : null}
      <Card>
        <div className="grid grid-cols-[1fr_auto_auto] items-center gap-x-5 border-b border-border px-4 py-2 text-xs font-medium text-fg-subtle">
          <span />
          <span className="w-10 text-center">{t('notifications.categories.push')}</span>
          <span className="w-10 text-center">{t('notifications.categories.email')}</span>
        </div>
        {rows.map((r) => {
          const label = t(`notifications.categories.label.${r.category}`)
          const emailCapable = EMAIL_CATEGORIES.has(r.category)
          return (
            <div key={r.category} data-testid={`notify-row-${r.category}`} className="grid grid-cols-[1fr_auto_auto] items-center gap-x-5 border-b border-border px-4 py-3 last:border-b-0">
              <span className="min-w-0 text-sm">{label}</span>
              <span className="flex w-10 justify-center">
                <Switch checked={r.push} onCheckedChange={(v) => toggle(r.category, 'push', v)} label={`${label} — ${t('notifications.categories.push')}`} testId={`notify-push-${r.category}`} />
              </span>
              <span className="flex w-10 justify-center" title={emailCapable ? undefined : t('notifications.categories.emailNotSent')}>
                <Switch
                  checked={emailCapable && r.email}
                  onCheckedChange={(v) => toggle(r.category, 'email', v)}
                  disabled={!emailAvailable || !emailCapable}
                  label={`${label} — ${t('notifications.categories.email')}`}
                  testId={`notify-email-${r.category}`}
                />
              </span>
            </div>
          )
        })}
      </Card>
    </section>
  )
}
