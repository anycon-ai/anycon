import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

export const packageRoot = fileURLToPath(new URL('..', import.meta.url))
export const repoRoot = fileURLToPath(new URL('../../..', import.meta.url))

export function fromHex(hex: string): Uint8Array {
  const clean = hex.trim()
  const out = new Uint8Array(clean.length / 2)
  for (let i = 0; i < out.length; i++) out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16)
  return out
}

export function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')
}

export interface VectorEntry {
  name: string
  type: 'envelope' | 'control' | 'rpc'
  value: Record<string, unknown>
}

export function loadVectors(): Array<VectorEntry & { bytes: Uint8Array }> {
  const dir = new URL('./vectors/', import.meta.url)
  const manifest = JSON.parse(readFileSync(new URL('manifest.json', dir), 'utf8')) as VectorEntry[]
  return manifest.map((entry) => ({
    ...entry,
    bytes: fromHex(readFileSync(new URL(`${entry.name}.hex`, dir), 'utf8')),
  }))
}

/** 递归按键名排序，模拟 generated.ts 里字母序的对象字面量。 */
export function sortKeysDeep<T>(value: T): T {
  if (Array.isArray(value)) return value.map(sortKeysDeep) as unknown as T
  if (value && typeof value === 'object' && !(value instanceof Uint8Array)) {
    const out: Record<string, unknown> = {}
    for (const key of Object.keys(value).toSorted()) {
      out[key] = sortKeysDeep((value as Record<string, unknown>)[key])
    }
    return out as T
  }
  return value
}
