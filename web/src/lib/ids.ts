/** uuid v4。优先用 crypto.randomUUID，没有（非安全上下文）时回退到 getRandomValues。 */
export function uuid(): string {
  const c = globalThis.crypto
  if (c && typeof c.randomUUID === 'function') return c.randomUUID()
  const b = new Uint8Array(16)
  c.getRandomValues(b)
  b[6] = (b[6]! & 0x0f) | 0x40
  b[8] = (b[8]! & 0x3f) | 0x80
  const h = Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`
}

/** URL 里的节点 id：前 8 位，冲突时回退全长（spec/web.md § 2）。 */
export function shortNodeId(id: string, all: readonly string[] = []): string {
  const short = id.slice(0, 8)
  const clash = all.some((o) => o !== id && o.startsWith(short))
  return clash ? id : short
}

/** 把 URL 里的节点参数解析回完整 id。 */
export function resolveNodeParam(param: string, all: readonly string[]): string | undefined {
  const exact = all.find((id) => id === param)
  if (exact) return exact
  const hits = all.filter((id) => id.startsWith(param))
  return hits.length === 1 ? hits[0] : undefined
}
