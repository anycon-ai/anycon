import type { ReactNode } from 'react'
import { Logo } from './Logo'

/** 登录 / 邀请 / 向导这类无侧栏的居中页面。 */
export function CenterLayout({ children, wide, medium }: { children: ReactNode; wide?: boolean; medium?: boolean }) {
  return (
    <div className="pt-safe pb-safe flex min-h-dvh flex-col items-center bg-bg-subtle px-4 py-8">
      <div className="mb-6 mt-4">
        <Logo className="text-lg" />
      </div>
      <main className={`w-full ${wide ? 'max-w-2xl' : medium ? 'max-w-md' : 'max-w-sm'}`}>{children}</main>
    </div>
  )
}
