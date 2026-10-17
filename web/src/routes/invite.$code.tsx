import { useEffect } from 'react'
import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { Spinner } from '@/ui/Misc'

// 邀请落地：把 code 存进 sessionStorage，转 /login（spec/web.md § 2）。
export const Route = createFileRoute('/invite/$code')({
  component: function InviteLanding() {
    const { code } = Route.useParams()
    const navigate = useNavigate()
    useEffect(() => {
      try {
        sessionStorage.setItem('invite', code)
      } catch (e) {
        // 隐私模式：邀请码丢了，登录页会让用户重新点链接
        console.warn('anycon: could not keep the invite code in sessionStorage', e)
      }
      void navigate({ to: '/login', replace: true })
    }, [code, navigate])
    return (
      <div className="flex min-h-dvh items-center justify-center">
        <Spinner className="size-6" />
      </div>
    )
  },
})
