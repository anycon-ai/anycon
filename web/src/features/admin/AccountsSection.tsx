// `/admin` 的账号管理（`multi`；`single` 也能看列表、授予 / 取消管理员）：搜索（名字 / 邮箱子串，或精确 id）、
// 「加载更多」（游标在 `X-Next-Cursor` 里）、授予 / 取消管理员、删账号（确认框）。
// 错误码（spec/hub.md § 2.4、§ 2.5）映射成文案：cannot_modify_self、bootstrap_admin、admin_by_config、not_found。
import { useEffect, useState } from 'react'
import { Search, ShieldCheck, ShieldOff, Trash2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useAdminAccounts, useAdminDeleteAccount, useSetAccountAdmin } from '@/api/accounts'
import type { ApiAdminAccount } from '@/api/types'
import { describeError } from '@/lib/errors'
import { formatDateTime } from '@/lib/format'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Input } from '@/ui/Field'
import { Card, Chip, SectionTitle, Skeleton } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

/** 搜索框停止输入多久后才发请求。 */
export const SEARCH_DEBOUNCE_MS = 300

export function AccountsSection({ meId }: { meId: string | undefined }) {
  const { t, i18n } = useTranslation()
  const [input, setInput] = useState('')
  const [q, setQ] = useState('')
  useEffect(() => {
    const id = setTimeout(() => setQ(input.trim()), SEARCH_DEBOUNCE_MS)
    return () => clearTimeout(id)
  }, [input])

  const accounts = useAdminAccounts(q)
  const setAdmin = useSetAccountAdmin()
  const del = useAdminDeleteAccount()
  const [target, setTarget] = useState<ApiAdminAccount | null>(null)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  const rows = accounts.data?.pages.flatMap((p) => p.items) ?? []

  return (
    <section data-testid="admin-accounts">
      <SectionTitle>{t('admin.accounts.title')}</SectionTitle>
      <div className="relative mb-3">
        <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-fg-subtle" aria-hidden />
        <Input type="search" value={input} onChange={(e) => setInput(e.target.value)} placeholder={t('admin.accounts.searchPlaceholder')} aria-label={t('admin.accounts.search')} className="pl-9" data-testid="admin-account-search" autoComplete="off" spellCheck={false} />
      </div>

      {accounts.isLoading ? <Skeleton className="h-14" /> : null}
      {accounts.isError ? (
        <p role="alert" className="text-sm text-danger">
          {describeError(accounts.error, t)}
        </p>
      ) : null}
      {accounts.data && rows.length === 0 ? <p className="text-sm text-fg-subtle">{q ? t('admin.accounts.noMatch') : t('admin.accounts.none')}</p> : null}

      <ul className="space-y-2" data-testid="admin-account-list" aria-busy={accounts.isFetching && !accounts.isFetchingNextPage}>
        {rows.map((a) => {
          const self = a.id === meId
          const roleBusy = setAdmin.isPending && setAdmin.variables?.id === a.id
          return (
            <li key={a.id} data-testid={`admin-account-${a.id}`}>
              <Card className="flex flex-wrap items-center gap-3 p-3">
                <div className="min-w-0 flex-1">
                  <p className="flex flex-wrap items-center gap-2 text-sm font-medium">
                    <span className="truncate">{a.display_name}</span>
                    {self ? <Chip tone="neutral">{t('admin.accounts.you')}</Chip> : null}
                    {a.is_admin ? <Chip tone="accent">{t('settings.admin')}</Chip> : null}
                    {a.admin_by_config ? (
                      <Chip tone="info" title={t('admin.accounts.byConfigHint')} testId={`admin-by-config-${a.id}`}>
                        {t('admin.accounts.byConfig')}
                      </Chip>
                    ) : null}
                  </p>
                  <p className="truncate text-xs text-fg-subtle">
                    {a.email || t('admin.accounts.noEmail')} · {t('admin.accounts.created', { when: formatDateTime(a.created_at_ms, i18n.language) })}
                  </p>
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  {a.is_admin ? (
                    <Button
                      size="sm"
                      variant="outline"
                      icon={<ShieldOff className="size-4" />}
                      loading={roleBusy}
                      disabled={self || a.admin_by_config || setAdmin.isPending}
                      title={a.admin_by_config ? t('admin.accounts.byConfigHint') : self ? t('errors.api.cannot_modify_self') : undefined}
                      data-testid={`admin-revoke-${a.id}`}
                      onClick={() => setAdmin.mutate({ id: a.id, grant: false }, { onSuccess: () => toast.success(t('admin.accounts.revoked', { name: a.display_name })), onError: (e) => toast.error(describeError(e, t)) })}
                    >
                      {t('admin.accounts.revokeAdmin')}
                    </Button>
                  ) : (
                    <Button
                      size="sm"
                      variant="outline"
                      icon={<ShieldCheck className="size-4" />}
                      loading={roleBusy}
                      disabled={self || setAdmin.isPending}
                      title={self ? t('errors.api.cannot_modify_self') : undefined}
                      data-testid={`admin-grant-${a.id}`}
                      onClick={() => setAdmin.mutate({ id: a.id, grant: true }, { onSuccess: () => toast.success(t('admin.accounts.granted', { name: a.display_name })), onError: (e) => toast.error(describeError(e, t)) })}
                    >
                      {t('admin.accounts.grantAdmin')}
                    </Button>
                  )}
                  <Button
                    size="sm"
                    variant="outline-danger"
                    icon={<Trash2 className="size-4" />}
                    disabled={self}
                    title={self ? t('errors.api.cannot_modify_self') : undefined}
                    aria-label={t('admin.accounts.delete')}
                    data-testid={`admin-delete-${a.id}`}
                    onClick={() => {
                      setDeleteError(null)
                      setTarget(a)
                    }}
                  >
                    <span className="hidden sm:inline">{t('admin.accounts.delete')}</span>
                  </Button>
                </div>
              </Card>
            </li>
          )
        })}
      </ul>

      {accounts.hasNextPage ? (
        <div className="mt-3 flex justify-center">
          <Button variant="outline" loading={accounts.isFetchingNextPage} onClick={() => void accounts.fetchNextPage()} data-testid="admin-accounts-more">
            {t('admin.accounts.loadMore')}
          </Button>
        </div>
      ) : null}

      <Dialog
        open={target !== null}
        onOpenChange={(o) => {
          if (!o && !del.isPending) setTarget(null)
        }}
        title={t('admin.accounts.deleteTitle', { name: target?.display_name ?? '' })}
        description={t('admin.accounts.deleteBody')}
        closeLabel={t('common.close')}
        testId="admin-delete-dialog"
      >
        <div className="space-y-4">
          {deleteError ? (
            <p role="alert" className="rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm text-danger" data-testid="admin-delete-error">
              {deleteError}
            </p>
          ) : null}
          <div className="flex flex-wrap justify-end gap-2">
            <Button variant="outline" disabled={del.isPending} onClick={() => setTarget(null)}>
              {t('common.cancel')}
            </Button>
            <Button
              variant="danger"
              loading={del.isPending}
              data-testid="admin-delete-confirm"
              onClick={() => {
                if (!target) return
                setDeleteError(null)
                del.mutate(target.id, {
                  onSuccess: () => {
                    toast.success(t('admin.accounts.deleted', { name: target.display_name }))
                    setTarget(null)
                  },
                  onError: (e) => setDeleteError(describeError(e, t)),
                })
              }}
            >
              {t('admin.accounts.deleteConfirm')}
            </Button>
          </div>
        </div>
      </Dialog>
    </section>
  )
}
