import type { EnrollHint } from '@/protocol'

// 控制字符（C0 / DEL / C1）、双向控制符和行 / 段分隔符：新设备自报的名字可以带着它们把界面上的文字顺序改掉
// eslint-disable-next-line no-control-regex
const UNSAFE = /[\u{0}-\u{1f}\u{7f}-\u{9f}\u{200e}\u{200f}\u{202a}-\u{202e}\u{2066}-\u{2069}\u{2028}\u{2029}]/gu

/** 对端自报的文字（设备名、平台）显示前的清理：去掉控制字符和双向控制符，空白折成一个，按字符（不是 UTF-16 单元）截断。 */
export function cleanHintText(value: unknown, max = 64): string {
  if (typeof value !== 'string') return ''
  return [...value.replace(UNSAFE, ' ').replace(/\s+/g, ' ').trim()].slice(0, max).join('')
}

export interface DisplayHint {
  /** 设备名；没有就是 `null`（调用方显示「未知设备」）。 */
  name: string | null
  platform: string | null
}

/** `EnrollInfo.hint` → 可以安全显示的名字和平台。hint 未经认证（hub 能改），只用来让人认出眼前的设备。 */
export function displayHint(hint: EnrollHint | null | undefined): DisplayHint {
  return { name: cleanHintText(hint?.name) || null, platform: cleanHintText(hint?.platform) || null }
}
