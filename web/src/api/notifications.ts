// Web Push 订阅和通知偏好的 hub REST（spec/hub.md § 2.3 / § 6，ADR-37）。REST 数据走 TanStack Query，不进 zustand。
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ApiError, apiFetch } from './client'
import type { ApiNotificationPref, ApiNotifications, ApiPushSubscribe, ApiPushSubscription, ApiPushTestResult } from './types'

export const notificationKeys = {
  prefs: ['me', 'notifications'] as const,
  pushSubs: ['push', 'subscriptions'] as const,
}

// ---- 推送订阅 ----

export const fetchPushSubscriptions = (): Promise<ApiPushSubscription[]> => apiFetch<ApiPushSubscription[]>('/api/push/subscriptions')

/** 上报（或顶掉）这台设备的订阅：同一设备 hub 只留一条。endpoint 不在白名单 → `400 push_endpoint_not_allowed`，设备不是本账号的 web 设备 → `404 device_not_found`。 */
export const registerPushSubscription = (body: ApiPushSubscribe): Promise<void> => apiFetch('/api/push/subscriptions', { method: 'POST', body })

/** 删订阅；已经不在了（404）当作完成。 */
export async function deletePushSubscription(id: string): Promise<void> {
  try {
    await apiFetch(`/api/push/subscriptions/${encodeURIComponent(id)}`, { method: 'DELETE' })
  } catch (e) {
    if (!(e instanceof ApiError && e.status === 404)) throw e
  }
}

/**
 * 把 hub 对测试推送的回应收成 `{ result: ok | gone | error, status? }`（hub.md § 2.3；`status` 是推送服务回的 HTTP 状态码，排查时有用）。
 * 回应来自网络，逐字段收窄：也接受裸字符串、`{ status | outcome: "ok" … }` 这些写法；空的 2xx 当作 `ok`，认不出的当作 `error`。
 */
export function parseTestResult(raw: unknown): ApiPushTestResult {
  const o = typeof raw === 'object' && raw !== null ? (raw as Record<string, unknown>) : null
  const word = typeof raw === 'string' ? raw : o ? [o.result, o.status, o.outcome].find((v) => typeof v === 'string') : undefined
  const status = o && typeof o.status === 'number' && Number.isInteger(o.status) && o.status >= 100 && o.status <= 599 ? o.status : undefined
  if (word === 'ok' || word === 'gone') return { result: word }
  if (word === 'error') return status === undefined ? { result: 'error' } : { result: 'error', status }
  return raw === undefined || raw === null || raw === '' ? { result: 'ok' } : status === undefined ? { result: 'error' } : { result: 'error', status }
}

export async function sendTestPush(deviceId: string): Promise<ApiPushTestResult> {
  const raw = await apiFetch<unknown>('/api/push/test', { method: 'POST', body: { device_id: deviceId } })
  return parseTestResult(raw)
}

export function usePushSubscriptions(enabled = true) {
  return useQuery({ queryKey: notificationKeys.pushSubs, queryFn: fetchPushSubscriptions, enabled })
}

export function useDeletePushSubscription() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => deletePushSubscription(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: notificationKeys.pushSubs }),
  })
}

// ---- 通知偏好 ----

export const fetchNotificationPrefs = (): Promise<ApiNotifications> => apiFetch<ApiNotifications>('/api/me/notifications')

export function useNotificationPrefs() {
  return useQuery({ queryKey: notificationKeys.prefs, queryFn: fetchNotificationPrefs })
}

/**
 * 保存偏好：开关先在界面上生效（乐观更新），`PUT` 失败就退回去。`PUT` 带整个 `categories` 数组（hub.md § 6.3：只认已知类别）。
 * `onSaved`：不管成败都调一次（让「页面在后台时的系统通知」下次重新读偏好）。
 */
export function useSaveNotificationPrefs(onSaved?: () => void) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (categories: ApiNotificationPref[]) => apiFetch('/api/me/notifications', { method: 'PUT', body: { categories } }),
    onMutate: async (categories) => {
      await qc.cancelQueries({ queryKey: notificationKeys.prefs })
      const prev = qc.getQueryData<ApiNotifications>(notificationKeys.prefs)
      if (prev) qc.setQueryData<ApiNotifications>(notificationKeys.prefs, { ...prev, categories })
      return { prev }
    },
    onError: (_e, _v, ctx) => {
      if (ctx?.prev) qc.setQueryData(notificationKeys.prefs, ctx.prev)
    },
    onSettled: () => {
      onSaved?.()
      return qc.invalidateQueries({ queryKey: notificationKeys.prefs })
    },
  })
}
