import { createFileRoute } from '@tanstack/react-router'
import { InvitePage } from '@/pages/InvitePage'

// 邀请落地：`single` 把 code 存进 sessionStorage 后转 `/login`；`multi` 告诉用户注册是开放的（spec/web.md § 2）。
export const Route = createFileRoute('/invite/$code')({
  component: function InviteLanding() {
    const { code } = Route.useParams()
    return <InvitePage code={code} />
  },
})
