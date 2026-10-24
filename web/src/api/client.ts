// hub HTTP 客户端（docs/spec/hub.md § 2）。同源；写操作带 `X-Anycon: 1`；401 → 回登录页。
export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly retryAfter?: number,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

let onUnauthorized: (() => void) | null = null

/** 登录态失效时的统一出口（router 挂上去做重定向）。 */
export function setUnauthorizedHandler(fn: (() => void) | null): void {
  onUnauthorized = fn
}

export interface ApiOptions {
  method?: 'GET' | 'POST' | 'PATCH' | 'PUT' | 'DELETE'
  body?: unknown
  signal?: AbortSignal
  /** 401 不触发全局重定向（登录请求本身）。 */
  noRedirect?: boolean
  /**
   * 用设备 token 认证（`Authorization: Bearer`），不带 cookie。hub 上只有少数接口认它（`POST /api/preview/tokens`，hub.md § 13.3）。
   * 这类请求的 401 说的是设备，不是登录态，所以不触发全局的回登录页。
   */
  bearer?: string
}

/** 带响应头的结果（分页接口的 `X-Next-Cursor` 在头里）。 */
export interface ApiResult<T> {
  data: T
  headers: Headers
}

export async function apiFetch<T = void>(path: string, opts: ApiOptions = {}): Promise<T> {
  return (await apiFetchRaw<T>(path, opts)).data
}

/** 和 `apiFetch` 一样，但同时返回响应头。 */
export async function apiFetchRaw<T = void>(path: string, opts: ApiOptions = {}): Promise<ApiResult<T>> {
  const method = opts.method ?? 'GET'
  const headers: Record<string, string> = { Accept: 'application/json' }
  if (method !== 'GET') {
    headers['Content-Type'] = 'application/json'
    headers['X-Anycon'] = '1'
  }
  if (opts.bearer !== undefined) headers.Authorization = `Bearer ${opts.bearer}`
  let res: Response
  try {
    res = await fetch(path, {
      method,
      headers,
      credentials: opts.bearer !== undefined ? 'omit' : 'same-origin',
      body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
      signal: opts.signal,
    })
  } catch (e) {
    if ((e as { name?: string }).name === 'AbortError') throw e
    throw new ApiError(0, 'network', 'network error')
  }
  if (!res.ok) {
    let code = `http_${res.status}`
    let message = res.statusText
    let retry: number | undefined
    try {
      const j = (await res.json()) as { error?: { code?: string; message?: string; retry_after?: number } }
      if (j.error?.code) code = j.error.code
      if (j.error?.message) message = j.error.message
      retry = j.error?.retry_after
    } catch {
      /* 非 JSON 错误体 */
    }
    // 401 + `invalid_code` 是「验证码不对」（绑定邮箱时也可能用 401 回），不是登录态失效：不能因此把人踢回登录页
    if (res.status === 401 && !opts.noRedirect && opts.bearer === undefined && code !== 'invalid_code') onUnauthorized?.()
    if (res.status === 429 && retry === undefined) {
      const h = Number(res.headers.get('Retry-After'))
      if (Number.isFinite(h) && h > 0) retry = h
    }
    throw new ApiError(res.status, res.status === 429 ? 'rate_limited' : code, message, retry)
  }
  if (res.status === 204) return { data: undefined as T, headers: res.headers }
  const text = await res.text()
  return { data: (text ? JSON.parse(text) : undefined) as T, headers: res.headers }
}
