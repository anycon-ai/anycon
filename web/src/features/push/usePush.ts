// 「本浏览器的推送」的状态和动作（设置页 / 通知）。浏览器 API 在 `./browser.ts`，开启 / 关闭的顺序在 `./actions.ts`。
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { useConfig } from '@/api/queries'
import { notificationKeys, sendTestPush, usePushSubscriptions } from '@/api/notifications'
import * as db from '@/db'
import { describeError } from '@/lib/errors'
import { useHub } from '@/stores/hub'
import { toast } from '@/stores/ui'
import { detectPushSupport, getLocalSubscription, notificationPermission } from './browser'
import { describePushError, disablePush, enablePush, reconcilePush } from './actions'

export type PushStatus = 'loading' | 'unsupported' | 'unavailable' | 'denied' | 'off' | 'on'
export type PushBusy = 'enable' | 'disable' | 'test' | null

/** 本浏览器在 hub 上的 web 设备 id：连上 hub 之后是 `auth_ok` 给的，没连上就读本地身份里存的；还没配对过是 null。 */
export function useWebDeviceId(): string | null {
  const live = useHub((s) => s.snapshot.deviceId)
  const [stored, setStored] = useState<string | null>(null)
  useEffect(() => {
    let alive = true
    db.getIdentity().then(
      (rec) => alive && setStored(rec?.device_id ?? null),
      () => undefined,
    )
    return () => {
      alive = false
    }
  }, [])
  return live ?? stored
}

export function usePush() {
  const { t } = useTranslation()
  const qc = useQueryClient()
  const config = useConfig()
  const deviceId = useWebDeviceId()
  const support = useMemo(() => detectPushSupport(), [])
  const [permission, setPermission] = useState(notificationPermission)
  // null = 还没读到
  const [subscribed, setSubscribed] = useState<boolean | null>(null)
  const [busy, setBusy] = useState<PushBusy>(null)
  const subs = usePushSubscriptions(support.ok)

  const refresh = useCallback(async (): Promise<void> => {
    setPermission(notificationPermission())
    if (!support.ok) return
    try {
      setSubscribed((await getLocalSubscription()) !== null)
    } catch (e) {
      console.warn('anycon: could not read the push subscription', e)
      setSubscribed(false)
    }
  }, [support.ok])

  useEffect(() => {
    void refresh()
    // 用户可能刚在浏览器的网站设置里改了权限：回到页面时重读
    const onBack = (): void => {
      if (document.visibilityState === 'visible') void refresh()
    }
    document.addEventListener('visibilitychange', onBack)
    window.addEventListener('focus', onBack)
    return () => {
      document.removeEventListener('visibilitychange', onBack)
      window.removeEventListener('focus', onBack)
    }
  }, [refresh])

  // 打开页面时对一次账：浏览器里有订阅、hub 里没有（或对不上）就补报（service worker 的 pushsubscriptionchange 可能没上报成）
  const reconciled = useRef(false)
  useEffect(() => {
    if (reconciled.current || !deviceId || !subs.data || subscribed !== true) return
    reconciled.current = true
    void reconcilePush({ deviceId, rows: subs.data }).then((did) => (did ? qc.invalidateQueries({ queryKey: notificationKeys.pushSubs }) : undefined))
  }, [deviceId, subs.data, subscribed, qc])

  const vapidKey = config.data?.vapid_public_key ?? null
  const hubReady = config.data ? config.data.push_available !== false && !!vapidKey : null
  const status: PushStatus = !support.ok
    ? 'unsupported'
    : config.isLoading || subscribed === null
      ? 'loading'
      : hubReady !== true
        ? 'unavailable'
        : permission === 'denied'
          ? 'denied'
          : subscribed
            ? 'on'
            : 'off'

  const run = async (kind: Exclude<PushBusy, null>, work: () => Promise<void>): Promise<void> => {
    setBusy(kind)
    try {
      await work()
    } catch (e) {
      toast.error(describePushError(e, t))
    } finally {
      setBusy(null)
      await refresh()
      await qc.invalidateQueries({ queryKey: notificationKeys.pushSubs })
    }
  }

  // 注意：enable 里 `Notification.requestPermission()` 必须是点击后的第一个 await（Safari 要求在用户手势里）
  const enable = (): Promise<void> => {
    if (!deviceId || !vapidKey) return Promise.resolve()
    return run('enable', async () => {
      await enablePush({ deviceId, vapidKey })
      // 刚亲手上报过了：页面手里那份订阅列表是开启之前读的、还没刷新，对账会拿它误判「hub 没有」而重复上报一次
      reconciled.current = true
      toast.success(t('notifications.push.enabled'))
    })
  }
  const disable = (): Promise<void> => {
    if (!deviceId) return Promise.resolve()
    return run('disable', async () => {
      await disablePush({ deviceId })
      toast.success(t('notifications.push.disabled'))
    })
  }
  const test = (): Promise<void> => {
    if (!deviceId) return Promise.resolve()
    return run('test', async () => {
      try {
        const { result, status } = await sendTestPush(deviceId)
        if (result === 'ok') toast.success(t('notifications.push.testSent'))
        else if (result === 'gone') toast.error(t('notifications.push.testGone'))
        // 推送服务回了状态码就带上（排查时有用：401/403 是密钥不对，413 载荷太大，429/5xx 是推送服务的问题）
        else toast.error(status === undefined ? t('notifications.push.testError') : t('notifications.push.testErrorStatus', { status }))
      } catch (e) {
        toast.error(describeError(e, t))
      }
    })
  }

  return { status, support, deviceId, busy, enable, disable, test, subscriptions: subs }
}
