import { createRootRouteWithContext, Outlet } from '@tanstack/react-router'
import type { RouterContext } from '@/router'
import { ThemeEffects } from '@/components/ThemeEffects'
import { Toaster } from '@/ui/Toaster'

export const Route = createRootRouteWithContext<RouterContext>()({
  component: () => (
    <>
      <ThemeEffects />
      <Outlet />
      <Toaster />
    </>
  ),
})
