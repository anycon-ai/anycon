import { useEffect, useState } from 'react'
import { Outlet } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { useConfig } from '@/api/queries'
import type { ApiMe } from '@/api/types'
import { startTransport } from '@/app/bootstrap'
import { useAuth } from '@/stores/auth'
import { useHub } from '@/stores/hub'
import { Button } from '@/ui/Button'
import { Spinner } from '@/ui/Misc'
import { Logo } from './Logo'

/** 登录后的门：启动传输层（载入 IndexedDB 身份与节点、连 hub），就绪后渲染子路由。 */
export function AppGate({ me }: { me: ApiMe }) {
  const { t } = useTranslation()
  const started = useHub((s) => s.started)
  const config = useConfig()
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    useAuth.getState().setMe(me)
  }, [me])
  useEffect(() => {
    if (config.data) useAuth.getState().setConfig(config.data)
  }, [config.data])

  useEffect(() => {
    setError(null)
    startTransport(me.id).catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [me.id])

  if (error)
    return (
      <div className="flex min-h-dvh flex-col items-center justify-center gap-4 p-6 text-center" role="alert">
        <Logo />
        <p className="max-w-sm text-sm text-danger">{t('app.startError', { error })}</p>
        <Button onClick={() => location.reload()}>{t('app.reload')}</Button>
      </div>
    )
  if (!started)
    return (
      <div className="flex min-h-dvh flex-col items-center justify-center gap-3" aria-busy="true">
        <Logo />
        <Spinner className="size-5" />
      </div>
    )
  return <Outlet />
}
