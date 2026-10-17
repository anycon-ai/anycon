import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { RouterProvider } from '@tanstack/react-router'
import { registerSW } from 'virtual:pwa-register'
import './i18n'
import './index.css'
import { ApiError } from './api/client'
import { fetchConfig, keys } from './api/queries'
import { listenForSwMessages } from './app/swMessages'
import { navigateTo, setNavigator } from './lib/navigate'
import { createAppRouter, installUnauthorizedHandler } from './router'
import { useUi } from './stores/ui'

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // 4xx（401 / 403 / 404 / 429）重试没有意义，别让登录跳转等在重试上
      retry: (count, error) => !(error instanceof ApiError && error.status >= 400 && error.status < 500) && count < 1,
      refetchOnWindowFocus: false,
      staleTime: 15_000,
    },
  },
})
// 启动时拉一次 hub 公开配置（登录方式、协议版本）
void queryClient.prefetchQuery({ queryKey: keys.config, queryFn: fetchConfig, staleTime: Infinity })
const router = createAppRouter(queryClient)

installUnauthorizedHandler(router, queryClient)
// 通知处理 / toast 的动作按钮这类非 React 代码要跳转页面时走这里（比如安全提醒 → 节点设置的「接入」标签）
setNavigator((to) => router.history.push(to))
// 点了系统通知：service worker 聚焦这个窗口并让它做站内跳转（ADR-37）
listenForSwMessages(navigateTo)

// PWA：新 service worker 就绪后不自动刷新，让用户在横幅上决定（spec/web.md § 8）。
const updateSW = registerSW({
  onNeedRefresh() {
    useUi.getState().setUpdateReady(() => void updateSW(true))
  },
})

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
)
