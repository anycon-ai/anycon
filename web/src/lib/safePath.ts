// 「站内路径」检查：通知点击、service worker 发给页面的跳转消息、`navigateTo` 都只接受同源的绝对路径。
//
// 光看前缀不够：WHATWG 的 URL 解析会先删掉 ASCII Tab / 换行，`"/\t/evil.example"` 按相对 URL 解析就成了 `https://evil.example/`。
// 所以三层：形状（单个 `/` 开头）→ 不含空白 / 控制字符 / 反斜杠 → 真的按浏览器的规则解析一遍，origin 必须不变。
// 百分号编码的（`/%09/evil`）是字面的路径字符，不会被当成分隔符，解析后仍是本站，放行。

/** 空白、C0 / C1 控制字符、DEL、反斜杠（逐字符判断，不写成带控制字符的正则：eslint 的 no-control-regex）。 */
function hasForbiddenChar(url: string): boolean {
  for (const ch of url) {
    const c = ch.codePointAt(0) ?? 0
    if (c <= 0x1f || (c >= 0x7f && c <= 0x9f) || ch === '\\' || /\s/u.test(ch)) return true
  }
  return false
}

/** `origin`：本站的 origin；页面和 service worker 里都是 `location.origin`。 */
export function isSafePath(url: unknown, origin: string = globalThis.location.origin): url is string {
  if (typeof url !== 'string') return false
  // 单个 `/` 开头：`//host`、`/\host` 是协议相对 URL / 浏览器会把 `\` 当 `/`
  if (!/^\/(?![/\\])/.test(url)) return false
  // 任何空白、C0 / C1 控制字符、DEL、反斜杠都不要（`\s` 已含 U+2028 / U+2029 / U+00A0 / U+FEFF）
  if (hasForbiddenChar(url)) return false
  try {
    return new URL(url, origin).origin === origin
  } catch {
    return false
  }
}
