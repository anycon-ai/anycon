// 状态横幅第 5 条「这个浏览器尚未启用通知」要不要显示（spec/web.md § 3.4）。
// 全都满足才显示：浏览器能收推送、hub 配了推送、这个浏览器配对过节点、本浏览器还没有推送订阅、浏览器没拒绝过通知、用户没关过横幅。
import { useEffect, useMemo, useState } from 'react'
import { useRouterState } from '@tanstack/react-router'
import { useConfig } from '@/api/queries'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { detectPushSupport, getRegistration, notificationPermission } from './browser'

/** 关掉（或点了「去开启」）之后不再显示：记在 localStorage 的偏好里。 */
export function dismissNotificationsPrompt(): void {
  usePrefs.getState().set({ notifyBannerDismissed: true })
}

export function useNotificationsPrompt(): boolean {
  const dismissed = usePrefs((s) => s.notifyBannerDismissed)
  const paired = useNodes((s) => s.order.length > 0)
  const config = useConfig()
  const pathname = useRouterState({ select: (s) => s.location.pathname })
  const support = useMemo(() => detectPushSupport(), [])
  const hubReady = !!config.data && config.data.push_available !== false && !!config.data.vapid_public_key
  const eligible = support.ok && paired && hubReady && !dismissed
  // 在哪个路径上读到的订阅状态：在通知设置页里开启之后离开，要等重新读过才判断，别用旧结果闪一下横幅
  const [seen, setSeen] = useState<{ path: string; subscribed: boolean } | null>(null)

  useEffect(() => {
    if (!eligible) return
    let alive = true
    const read = async (): Promise<void> => {
      try {
        const reg = await getRegistration()
        // 没有注册 service worker（开发服务器）：开不了推送，也就不催
        const subscribed = reg ? (await reg.pushManager.getSubscription()) !== null : true
        if (alive) setSeen({ path: pathname, subscribed })
      } catch (e) {
        console.warn('anycon: could not read the push subscription', e)
      }
    }
    void read()
    return () => {
      alive = false
    }
  }, [eligible, pathname])

  if (!eligible || pathname.startsWith('/settings/notifications')) return false
  // 用户在浏览器里明确拒绝了：催也没用，通知页会说明怎么改
  if (notificationPermission() === 'denied') return false
  return seen?.path === pathname && !seen.subscribed
}
