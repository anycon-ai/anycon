import { useState } from 'react'
import { Plus, Trash2 } from 'lucide-react'
import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import { useAdminStatus, useConfig, useCreateInvite, useDeleteInvite, useInvites, useMe } from '@/api/queries'
import type { ApiInviteCreated } from '@/api/types'
import { PageHeader } from '@/components/PageHeader'
import { AccountsSection } from '@/features/admin/AccountsSection'
import { describeError } from '@/lib/errors'
import { formatBytes, formatDateTime, formatDuration } from '@/lib/format'
import { isMultiMode } from '@/lib/oauth'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Field, Input, Select } from '@/ui/Field'
import { Card, Chip, CopyButton, EmptyState, SectionTitle, Skeleton } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

/**
 * `/admin`：账号管理（搜索、授予 / 取消管理员、删除）、邀请链接的生成 / 复制 / 撤销（只有 `single`，`multi` 没有邀请）、hub 状态。
 */
export function AdminPage() {
  const { t, i18n } = useTranslation()
  const me = useMe()
  const config = useConfig()
  const isAdmin = me.data?.is_admin ?? false
  // 配置没到之前不知道有没有邀请：先不拉（`multi` 下 `/api/invites` 是 404）
  const invitesEnabled = config.data !== undefined && !isMultiMode(config.data)
  const invites = useInvites(isAdmin && invitesEnabled)
  const status = useAdminStatus(isAdmin)
  const create = useCreateInvite()
  const del = useDeleteInvite()
  const [open, setOpen] = useState(false)
  const [note, setNote] = useState('')
  const [ttl, setTtl] = useState('168')
  const [created, setCreated] = useState<ApiInviteCreated | null>(null)

  if (me.data && !isAdmin)
    return (
      <>
        <PageHeader crumbs={[{ label: t('admin.title') }]} showNodeStatus={false} />
        <EmptyState title={t('admin.forbiddenTitle')}>{t('admin.forbiddenBody')}</EmptyState>
      </>
    )

  return (
    <>
      <PageHeader crumbs={[{ label: t('admin.title') }]} showNodeStatus={false} />
      <div className="scroll-thin min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-3xl space-y-8 p-4 md:p-6">
          <h1 className="text-xl font-semibold">{t('admin.title')}</h1>

          {invitesEnabled ? (
            <section data-testid="admin-invites">
              <SectionTitle
                action={
                  <Button size="sm" variant="primary" icon={<Plus className="size-4" />} onClick={() => { setNote(''); setCreated(null); setOpen(true) }} data-testid="invite-new">
                    {t('admin.inviteNew')}
                  </Button>
                }
              >
                {t('admin.invites')}
              </SectionTitle>
              <p className="mb-3 text-sm text-fg-muted">{t('admin.invitesHint')}</p>
              {invites.isLoading ? <Skeleton className="h-14" /> : null}
              {invites.data && invites.data.length === 0 ? <p className="text-sm text-fg-subtle">{t('admin.noInvites')}</p> : null}
              <ul className="space-y-2" data-testid="invite-list">
                {(invites.data ?? []).map((inv) => {
                  const expired = inv.expires_at_ms < Date.now()
                  return (
                    <li key={inv.id}>
                      <Card className="flex items-center gap-3 p-3">
                        <div className="min-w-0 flex-1">
                          <p className="flex flex-wrap items-center gap-2 text-sm font-medium">
                            <span className="truncate">{inv.note || t('admin.noNote')}</span>
                            {inv.used ? <Chip tone="success">{t('admin.used')}</Chip> : expired ? <Chip tone="neutral">{t('admin.expired')}</Chip> : <Chip tone="info">{t('admin.pending')}</Chip>}
                          </p>
                          <p className="text-xs text-fg-subtle">{t('admin.expires', { when: formatDateTime(inv.expires_at_ms, i18n.language) })}</p>
                        </div>
                        <Button size="sm" variant="outline-danger" icon={<Trash2 className="size-4" />} onClick={() => del.mutate(inv.id, { onError: (e) => toast.error(describeError(e, t)) })} aria-label={t('admin.revoke')}>
                          <span className="hidden sm:inline">{t('admin.revoke')}</span>
                        </Button>
                      </Card>
                    </li>
                  )
                })}
              </ul>
            </section>
          ) : null}

          {isAdmin ? <AccountsSection meId={me.data?.id} /> : null}

          <section>
            <SectionTitle>{t('admin.status')}</SectionTitle>
            {status.data ? (
              <Card className="divide-y divide-border" testId="admin-status">
                {Object.entries(status.data).map(([k, v]) => (
                  <div key={k} className="flex items-center justify-between gap-4 px-4 py-2.5 text-sm">
                    <span className="text-fg-muted">{t(`admin.statusKeys.${k}`, { defaultValue: k })}</span>
                    <span className="min-w-0 truncate font-mono text-xs">{formatStatus(k, v, i18n.language, t)}</span>
                  </div>
                ))}
              </Card>
            ) : status.isLoading ? (
              <Skeleton className="h-24" />
            ) : null}
          </section>
        </div>
      </div>

      <Dialog open={open} onOpenChange={setOpen} title={created ? t('admin.inviteCreated') : t('admin.inviteNew')} closeLabel={t('common.close')}>
        {created ? (
          <div className="space-y-3" data-testid="invite-created">
            <p className="text-sm text-warning">{t('admin.inviteOnce')}</p>
            <code className="block break-all rounded-md bg-bg-muted px-3 py-2 font-mono text-sm" data-testid="invite-url">
              {created.url}
            </code>
            <div className="flex justify-between">
              <CopyButton text={created.url} />
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
              create.mutate({ note: note.trim() || undefined, ttl_hours: Number(ttl) }, { onSuccess: setCreated, onError: (err) => toast.error(describeError(err, t)) })
            }}
          >
            <Field label={t('admin.note')} htmlFor="invite-note">
              <Input id="invite-note" value={note} onChange={(e) => setNote(e.target.value)} placeholder={t('admin.notePlaceholder')} autoFocus maxLength={120} />
            </Field>
            <Field label={t('admin.ttl')} htmlFor="invite-ttl">
              <Select id="invite-ttl" value={ttl} onChange={(e) => setTtl(e.target.value)}>
                <option value="24">{t('admin.ttl1d')}</option>
                <option value="168">{t('admin.ttl7d')}</option>
                <option value="720">{t('admin.ttl30d')}</option>
              </Select>
            </Field>
            <div className="flex justify-end">
              <Button type="submit" variant="primary" loading={create.isPending}>
                {t('admin.inviteCreate')}
              </Button>
            </div>
          </form>
        )}
      </Dialog>
    </>
  )
}

/** hub 状态里的值：时间戳 / 时长 / 字节数按名字约定格式化，其余原样显示。 */
export function formatStatus(key: string, v: unknown, lang: string, t: TFunction): string {
  if (typeof v === 'number') {
    if (key.endsWith('_at_ms')) return formatDateTime(v, lang)
    if (key.endsWith('uptime_ms')) return formatDuration(v)
    if (key.endsWith('_bytes')) return formatBytes(v)
  }
  if (typeof v === 'boolean') return v ? t('common.yes') : t('common.no')
  return typeof v === 'object' ? JSON.stringify(v) : String(v)
}
