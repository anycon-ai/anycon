// hub REST 的 TanStack Query 封装。REST 数据不进 zustand。
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { revokeDevice } from '@/features/devices/revoke'
import { forgetNode } from '@/features/devices/removeNode'
import { apiFetch } from './client'
import type { ApiAdminStatus, ApiBrowserSession, ApiConfig, ApiDevice, ApiInvite, ApiInviteCreated, ApiMe, ApiToken } from './types'

export const keys = {
  config: ['config'] as const,
  me: ['me'] as const,
  tokens: ['me', 'tokens'] as const,
  sessions: ['me', 'sessions'] as const,
  devices: ['devices'] as const,
  invites: ['invites'] as const,
  adminStatus: ['admin', 'status'] as const,
}

export const fetchConfig = (): Promise<ApiConfig> => apiFetch<ApiConfig>('/api/config', { noRedirect: true })
export const fetchMe = (): Promise<ApiMe> => apiFetch<ApiMe>('/api/me', { noRedirect: true })

export function useConfig() {
  return useQuery({ queryKey: keys.config, queryFn: fetchConfig, staleTime: Infinity, retry: 1 })
}
export function useMe() {
  return useQuery({ queryKey: keys.me, queryFn: fetchMe, retry: false, staleTime: 60_000 })
}

export const loginWithToken = (token: string): Promise<void> => apiFetch('/auth/token', { method: 'POST', body: { token }, noRedirect: true })
export const loginWithInvite = (code: string, display_name?: string): Promise<void> => apiFetch('/auth/invite', { method: 'POST', body: { code, display_name }, noRedirect: true })
export const logout = (): Promise<void> => apiFetch('/auth/logout', { method: 'POST', noRedirect: true })

/** 邮箱规范化（和 hub 一致：去首尾空白 + 小写）。 */
export const normalizeEmail = (email: string): string => email.trim().toLowerCase()
/** hub 回的「多少秒之后可以再要一个码」（`resend_after_secs`）；没有或不合理就按文档的 60 秒。 */
export function resendAfterSecs(raw: unknown): number {
  const v = typeof raw === 'object' && raw !== null ? (raw as Record<string, unknown>).resend_after_secs : undefined
  return typeof v === 'number' && Number.isFinite(v) && v >= 1 && v <= 3600 ? Math.ceil(v) : 60
}
/** 邮箱登录第一步（ADR-37）：hub **总是** 200，不告诉你这个邮箱有没有绑定账号。返回多少秒之后才能重发。 */
export const sendLoginCode = async (email: string): Promise<number> => resendAfterSecs(await apiFetch<unknown>('/auth/email/send', { method: 'POST', body: { email: normalizeEmail(email) }, noRedirect: true }))
/** 邮箱登录第二步：成功设 cookie；码不对 / 过期 / 不存在一律 `401 invalid_code`。 */
export const loginWithEmail = (email: string, code: string): Promise<void> => apiFetch('/auth/email/verify', { method: 'POST', body: { email: normalizeEmail(email), code }, noRedirect: true })

export function usePatchMe() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (display_name: string) => apiFetch<ApiMe | void>('/api/me', { method: 'PATCH', body: { display_name } }),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.me }),
  })
}

/** 绑定 / 更换邮箱，第一步：给新邮箱发码（已被别的账号占用 → `409 email_taken`）。 */
export function useRequestEmailCode() {
  return useMutation({ mutationFn: async (email: string): Promise<number> => resendAfterSecs(await apiFetch<unknown>('/api/me/email', { method: 'POST', body: { email: normalizeEmail(email) } })) })
}
/** 第二步：验证码对了邮箱才生效。 */
export function useVerifyEmail() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (v: { email: string; code: string }) => apiFetch('/api/me/email/verify', { method: 'POST', body: { email: normalizeEmail(v.email), code: v.code } }),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.me }),
  })
}
/** 解绑（账号没有别的登录方式时 `409 last_login_method`）。 */
export function useUnbindEmail() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: () => apiFetch('/api/me/email', { method: 'DELETE' }),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.me }),
  })
}

/** 登录 token 只有 `single` 模式有；`multi` 下这些接口是 404，所以调用方要在 `multi` 里传 `enabled = false`。 */
export function useTokens(enabled = true) {
  return useQuery({ queryKey: keys.tokens, queryFn: () => apiFetch<ApiToken[]>('/api/me/tokens'), enabled })
}
export function useCreateToken() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (label: string) => apiFetch<{ id: string; token: string }>('/api/me/tokens', { method: 'POST', body: { label } }),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.tokens }),
  })
}
export function useDeleteToken() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => apiFetch(`/api/me/tokens/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.tokens }),
  })
}

export function useBrowserSessions() {
  return useQuery({ queryKey: keys.sessions, queryFn: () => apiFetch<ApiBrowserSession[]>('/api/me/sessions') })
}
export function useRevokeBrowserSession() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => apiFetch(`/api/me/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.sessions }),
  })
}

/**
 * 账号下的全部设备。`enabled = false`：条件满足之前不请求（比如只有找不到本地节点时才需要）；
 * `refetchMs`：页面开着期间定时刷新（解锁页等节点上线）。
 */
export function useDevices(opts: { enabled?: boolean; refetchMs?: number } = {}) {
  return useQuery({ queryKey: keys.devices, queryFn: () => apiFetch<ApiDevice[]>('/api/devices'), enabled: opts.enabled ?? true, refetchInterval: opts.refetchMs ?? false })
}
export function useRenameDevice() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (v: { id: string; name: string }) => apiFetch(`/api/devices/${encodeURIComponent(v.id)}`, { method: 'PATCH', body: { name: v.name } }),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.devices }),
  })
}
export function useRevokeDevice() {
  const qc = useQueryClient()
  return useMutation({
    // 节点那一步（node.revoke_device）+ hub 那一步，见 features/devices/revoke.ts
    mutationFn: (v: { id: string; nodeIds: string[] }) => revokeDevice(v.id, v.nodeIds),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.devices }),
  })
}

/**
 * 从账号里移除一台节点，离线也能做（机器重装了、旧的 daemon 再也不会上线）：hub 吊销它（断开连接、删掉所有配对；hub 已经没有它时当作完成），
 * 再在这个浏览器里忘记它。hub 那一步失败就不忘记，可以重试。先刷新设备列表再忘记：否则页面守卫拿缓存的旧列表，会把它当成「账号下、还没解锁」的节点跳去解锁页。
 */
export function useRemoveNode() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: async (nodeId: string) => {
      await revokeDevice(nodeId, [])
      await qc.invalidateQueries({ queryKey: keys.devices })
      await forgetNode(nodeId)
    },
  })
}

export function useInvites(enabled = true) {
  return useQuery({ queryKey: keys.invites, queryFn: () => apiFetch<ApiInvite[]>('/api/invites'), enabled })
}
export function useCreateInvite() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (v: { note?: string; ttl_hours?: number }) => apiFetch<ApiInviteCreated>('/api/invites', { method: 'POST', body: v }),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.invites }),
  })
}
export function useDeleteInvite() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => apiFetch(`/api/invites/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.invites }),
  })
}

export function useAdminStatus(enabled = true) {
  return useQuery({ queryKey: keys.adminStatus, queryFn: () => apiFetch<ApiAdminStatus>('/api/admin/status'), enabled })
}
