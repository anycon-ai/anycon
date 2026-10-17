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
}

export async function apiFetch<T = void>(path: string, opts: ApiOptions = {}): Promise<T> {
  const method = opts.method ?? 'GET'
  const headers: Record<string, string> = { Accept: 'application/json' }
  if (method !== 'GET') {
    headers['Content-Type'] = 'application/json'
    headers['X-Anycon'] = '1'
  }
  let res: Response
  try {
    res = await fetch(path, {
      method,
      headers,
      credentials: 'same-origin',
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
    if (res.status === 401 && !opts.noRedirect && code !== 'invalid_code') onUnauthorized?.()
    if (res.status === 429 && retry === undefined) {
      const h = Number(res.headers.get('Retry-After'))
      if (Number.isFinite(h) && h > 0) retry = h
    }
    throw new ApiError(res.status, res.status === 429 ? 'rate_limited' : code, message, retry)
  }
  if (res.status === 204) return undefined as T
  const text = await res.text()
  return (text ? JSON.parse(text) : undefined) as T
}
