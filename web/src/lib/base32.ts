// RFC 4648 base32：小写字母表、不带填充。接入二维码里的扫码密钥（ADR-36）用它，daemon / CLI 用同一个字母表。
const ALPHABET = 'abcdefghijklmnopqrstuvwxyz234567'
const LOOKUP: Record<string, number> = Object.fromEntries([...ALPHABET].map((c, i) => [c, i]))

/** 字节 → 小写 base32（无填充）。32 字节 → 52 个字符。 */
export function base32Encode(bytes: Uint8Array): string {
  let out = ''
  let buffer = 0
  let bits = 0
  for (const b of bytes) {
    buffer = (buffer << 8) | b
    bits += 8
    while (bits >= 5) {
      out += ALPHABET[(buffer >>> (bits - 5)) & 31]
      bits -= 5
    }
    buffer &= (1 << bits) - 1
  }
  if (bits > 0) out += ALPHABET[(buffer << (5 - bits)) & 31]
  return out
}

/**
 * 小写 base32（无填充）→ 字节。严格：只认小写字母表，不接受填充和空白，
 * 末尾多出来的位必须是 0（规范编码，同一串字节只有一种写法）。不合法返回 null。
 */
export function base32Decode(text: string): Uint8Array | null {
  const out: number[] = []
  let buffer = 0
  let bits = 0
  for (const ch of text) {
    const v = LOOKUP[ch]
    if (v === undefined) return null
    buffer = (buffer << 5) | v
    bits += 5
    if (bits >= 8) {
      out.push((buffer >>> (bits - 8)) & 0xff)
      bits -= 8
    }
    buffer &= (1 << bits) - 1
  }
  // 余下不足一字节的位必须全是 0；余 5 位及以上说明长度不可能是某个字节数的编码
  if (bits >= 5 || buffer !== 0) return null
  return Uint8Array.from(out)
}
