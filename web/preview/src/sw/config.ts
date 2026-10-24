// `/__anycon/config.json`（hub 提供）：`{ label, hub_origin, protocol_version }`。service worker 只用 `hub_origin`
// 来拼 `frame-ancestors`。读到一次就缓存；读不到（hub 暂时不可达）不缓存失败，下次再试。
export interface PreviewConfig {
  hubOrigin?: string
}

const URL_PATH = '/__anycon/config.json'
const RETRY_AFTER_FAILURE_MS = 30_000

export function createConfigLoader(
  fetchFn: (input: string) => Promise<Response> = (input) => fetch(input, { cache: 'no-store' }),
  now: () => number = Date.now,
): () => Promise<PreviewConfig> {
  let cached: PreviewConfig | undefined
  let failedAt = -Infinity
  let inflight: Promise<PreviewConfig> | undefined

  async function load(): Promise<PreviewConfig> {
    try {
      const res = await fetchFn(URL_PATH)
      if (!res.ok) throw new Error(`config.json: ${res.status}`)
      const body: unknown = await res.json()
      const origin = typeof body === 'object' && body !== null ? (body as Record<string, unknown>).hub_origin : undefined
      const hubOrigin = typeof origin === 'string' && /^https?:\/\/[^\s/;,]+$/i.test(origin) ? origin : undefined
      if (!hubOrigin) throw new Error('config.json: no valid hub_origin')
      cached = { hubOrigin }
      return cached
    } catch {
      failedAt = now()
      return {}
    }
  }

  return () => {
    if (cached) return Promise.resolve(cached)
    if (now() - failedAt < RETRY_AFTER_FAILURE_MS) return Promise.resolve({})
    inflight ??= load().finally(() => {
      inflight = undefined
    })
    return inflight
  }
}
