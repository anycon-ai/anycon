// 来自 agent / 外部命令输出的链接：只有 http(s) 才渲染成可点的链接，别的协议（javascript:、data: …）一律当文字。

/** 是 `http:` / `https:` 的 URL 就返回规整后的地址，否则（含解析失败）返回 null。 */
export function safeHttpUrl(url: string): string | null {
  try {
    const u = new URL(url)
    return u.protocol === 'https:' || u.protocol === 'http:' ? u.href : null
  } catch {
    return null
  }
}
