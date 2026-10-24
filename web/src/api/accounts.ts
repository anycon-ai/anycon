// 账号相关的 hub REST（spec/hub.md § 2.3 / § 2.4）：OAuth 身份、配额、删账号、管理员的账号管理。
// 管理员的账号列表是 keyset 分页：下一页的游标在 `X-Next-Cursor` 响应头里（spec/hub.md § 2.5）。
import { keepPreviousData, useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { apiFetch, apiFetchRaw } from './client'
import { keys } from './queries'
import type { ApiAdminAccount, ApiIdentity, ApiQuota, OAuthProvider } from './types'

export const accountKeys = {
  identities: ['me', 'identities'] as const,
  quota: ['quota'] as const,
  adminAccounts: ['admin', 'accounts'] as const,
}

// ---- OAuth 身份 ----

export function useIdentities(enabled = true) {
  return useQuery({ queryKey: accountKeys.identities, queryFn: () => apiFetch<ApiIdentity[]>('/api/me/identities'), enabled })
}

/** 解除关联；账号的最后一种登录方式不能解除（`409 last_login_method`）。没关联过也是 204。 */
export function useUnlinkIdentity() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (provider: OAuthProvider) => apiFetch(`/api/me/identities/${encodeURIComponent(provider)}`, { method: 'DELETE' }),
    // `/api/me.login_methods` 随之变化，「最后一种登录方式」的判断要用新值
    onSuccess: () => Promise.all([qc.invalidateQueries({ queryKey: accountKeys.identities }), qc.invalidateQueries({ queryKey: keys.me })]),
  })
}

// ---- 配额 ----

export function useQuota(enabled = true) {
  return useQuery({ queryKey: accountKeys.quota, queryFn: () => apiFetch<ApiQuota>('/api/quota'), enabled })
}

// ---- 删账号 ----

/**
 * 删除自己的账号。要求登录会话不超过 10 分钟（否则 `403 reauth_required`）；`single` 的引导管理员 `409 bootstrap_admin`。
 * 成功后 cookie 已被 hub 清掉，本地数据的清理由调用方负责。
 */
export const deleteMyAccount = (): Promise<void> => apiFetch('/api/me', { method: 'DELETE' })

export function useDeleteMyAccount() {
  return useMutation({ mutationFn: deleteMyAccount })
}

// ---- 管理员：账号 ----

export const ADMIN_ACCOUNTS_PAGE_SIZE = 50

interface AccountsPage {
  items: ApiAdminAccount[]
  /** 下一页的游标；没有下一页是 null。 */
  next: string | null
}

/** `q`：账号名 / 邮箱的子串（不分大小写），或精确的账号 id；空串 = 不过滤。 */
export function useAdminAccounts(q: string, enabled = true) {
  return useInfiniteQuery({
    queryKey: [...accountKeys.adminAccounts, q] as const,
    enabled,
    initialPageParam: null as string | null,
    queryFn: async ({ pageParam }): Promise<AccountsPage> => {
      const params = new URLSearchParams({ limit: String(ADMIN_ACCOUNTS_PAGE_SIZE) })
      if (q) params.set('q', q)
      if (pageParam) params.set('cursor', pageParam)
      const { data, headers } = await apiFetchRaw<ApiAdminAccount[]>(`/api/admin/accounts?${params.toString()}`)
      return { items: data, next: headers.get('X-Next-Cursor') }
    },
    getNextPageParam: (last) => last.next,
    // 搜索词变了先留着上一批结果，免得列表每敲一个字就闪成骨架
    placeholderData: keepPreviousData,
  })
}

/** 授予（`grant = true`）或取消（`false`）库里的管理员标记。回的是更新后的账号行。 */
export function useSetAccountAdmin() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (v: { id: string; grant: boolean }) => apiFetch<ApiAdminAccount>(`/api/admin/accounts/${encodeURIComponent(v.id)}/admin`, { method: v.grant ? 'POST' : 'DELETE' }),
    onSuccess: () => qc.invalidateQueries({ queryKey: accountKeys.adminAccounts }),
  })
}

export function useAdminDeleteAccount() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => apiFetch(`/api/admin/accounts/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: () => Promise.all([qc.invalidateQueries({ queryKey: accountKeys.adminAccounts }), qc.invalidateQueries({ queryKey: keys.adminStatus })]),
  })
}
