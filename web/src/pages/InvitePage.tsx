import { useEffect } from 'react'
import { Link, useNavigate } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { useConfig } from '@/api/queries'
import { CenterLayout } from '@/components/CenterLayout'
import { stashInvite } from '@/lib/invite'
import { isMultiMode } from '@/lib/oauth'
import { Button } from '@/ui/Button'
import { Card, Spinner } from '@/ui/Misc'

/**
 * `/invite/:code`（spec/web.md § 2）。
 * - `single`：把邀请码暂存进 sessionStorage，转 `/login`。登录页发现暂存的码就显示「接受邀请」，并让 GitHub / Google 按钮带上
 *   `?invite=<code>`（`/auth/oauth/<p>/start?invite=`），新用户用 OAuth 登录时由 hub 兑换这个邀请。
 * - `multi`：没有邀请这回事，注册是开放的，说一声并引到 `/login`；码不存。
 * hub 的配置拉不到时也照旧转 `/login`（那里会提示配置加载失败）。
 */
export function InvitePage({ code }: { code: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const config = useConfig()
  const multi = isMultiMode(config.data)
  const settled = !config.isLoading
  useEffect(() => {
    if (!settled || multi) return
    stashInvite(code)
    void navigate({ to: '/login', replace: true })
  }, [settled, multi, code, navigate])

  if (!settled || !multi)
    return (
      <div className="flex min-h-dvh items-center justify-center">
        <Spinner className="size-6" />
      </div>
    )
  return (
    <CenterLayout>
      <Card className="p-5" testId="invite-open-registration">
        <h1 className="text-lg font-semibold">{t('invite.openTitle')}</h1>
        <p className="mt-1 text-sm text-fg-muted">{t('invite.openBody')}</p>
        <Link to="/login" className="mt-5 block">
          <Button variant="primary" className="w-full" data-testid="invite-go-login">
            {t('invite.openAction')}
          </Button>
        </Link>
      </Card>
    </CenterLayout>
  )
}
