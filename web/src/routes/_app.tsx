import { createFileRoute, redirect } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { ApiError } from '@/api/client'
import { fetchMe, keys } from '@/api/queries'
import { AppGate } from '@/components/AppGate'
import { clearInvite } from '@/lib/invite'
import { loginRedirectTarget } from '@/lib/loginRedirect'
import { timed } from '@/lib/timing'
import { Button } from '@/ui/Button'
import { Logo } from '@/components/Logo'

export const Route = createFileRoute('/_app')({
  beforeLoad: async ({ context, location }) => {
    try {
      const me = await context.queryClient.ensureQueryData({ queryKey: keys.me, queryFn: () => timed('http /api/me', fetchMe), staleTime: 60_000 })
      // 已经登录了：暂存的邀请码不再需要（走 OAuth 兑换邀请时不经过登录页，没人替它清）
      clearInvite()
      return { me }
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) {
        // redirect 只带路径，不带 hash：配对链接的 `#code=` 不能进 query（docs/protocol.md § 4.1）；短码由 loginRedirectTarget 暂存
        throw redirect({ to: '/login', search: { redirect: loginRedirectTarget(location) }, replace: true })
      }
      throw e
    }
  },
  component: function AppRoute() {
    const { me } = Route.useRouteContext()
    return <AppGate me={me} />
  },
  errorComponent: AppError,
})

function AppError({ error, reset }: { error: unknown; reset: () => void }) {
  const { t } = useTranslation()
  return (
    <div className="flex min-h-dvh flex-col items-center justify-center gap-4 p-6 text-center" role="alert">
      <Logo />
      <p className="max-w-sm text-sm text-danger">{error instanceof ApiError && error.status === 0 ? t('app.hubUnreachable') : error instanceof Error ? error.message : String(error)}</p>
      <Button onClick={reset}>{t('app.retry')}</Button>
    </div>
  )
}
