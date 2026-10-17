import { PAIR_CODE_ALPHABET, PAIR_CODE_LEN, PAIR_CODE_LEN_LONG } from './constants'

/** 规范化短码：大写，去掉连字符和空白。 */
export function normalizePairCode(input: string): string {
  return input.replace(/[\s-]/g, '').toUpperCase()
}

/**
 * 输入框格式化：规范化后最多保留 16 位，每 4 位插一个连字符。
 * 8 位短码显示成 `XXXX-XXXX`，16 位长码（链接 / 二维码用）显示成 `XXXX-XXXX-XXXX-XXXX`。
 */
export function formatPairCodeInput(input: string): string {
  const n = normalizePairCode(input).slice(0, PAIR_CODE_LEN_LONG)
  return n.replace(/(.{4})(?=.)/g, '$1-')
}

/** 规范化后的短码是不是合法的 8 位或 16 位（字母表内）。 */
export function isValidPairCode(normalized: string): boolean {
  return (normalized.length === PAIR_CODE_LEN || normalized.length === PAIR_CODE_LEN_LONG) && [...normalized].every((c) => PAIR_CODE_ALPHABET.includes(c))
}

/** 输入框里的短码是否完整（8 位或 16 位，字母表内）。 */
export function isCompletePairCode(input: string): boolean {
  return isValidPairCode(normalizePairCode(input))
}

/** `decodeURIComponent` 遇到非法的 `%`（比如粘贴进来半截 `%E4%`）会抛 URIError：这里退回原文，让后面的格式化 / 校验去处理。 */
function safeDecode(s: string): string {
  try {
    return decodeURIComponent(s)
  } catch {
    return s
  }
}

/**
 * 从配对链接（`…/pair#code=XXXX-XXXX`，兼容旧的 `?code=`）、`#code=…` 片段或纯文本里取短码。
 * 短码放在 fragment 里是为了不让浏览器把它发给 hub。
 */
export function extractPairCode(raw: string): string {
  const text = raw.trim()
  try {
    const u = new URL(text)
    const c = new URLSearchParams(u.hash.replace(/^#/, '')).get('code') ?? u.searchParams.get('code')
    if (c) return formatPairCodeInput(c)
  } catch {
    /* 不是完整 URL，下面按片段 / 纯文本处理 */
  }
  const frag = /(?:^|[#?&])code=([^&#\s]+)/.exec(text)
  if (frag?.[1]) return formatPairCodeInput(safeDecode(frag[1]))
  return formatPairCodeInput(text)
}

/** 当前页面 URL 里带的短码（`location.hash` 的 `#code=…`，兼容旧的 `?code=`），没有返回 ''。 */
export function pairCodeFromLocation(loc: Pick<Location, 'hash' | 'search'>): string {
  const h = new URLSearchParams(loc.hash.replace(/^#/, '')).get('code')
  const q = new URLSearchParams(loc.search).get('code')
  const raw = h ?? q
  return raw ? formatPairCodeInput(raw) : ''
}
