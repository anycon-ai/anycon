// 导航响应（HTML）里注入 shim 的 `<script>`（spec/web.md § 13.2「注入 shim」）。
//
// 流式：只缓冲到找到插入点为止（最多 64 KiB），之后的块原样透传。插入点是第一个 `<head>` 之后；没有 `<head>` 就放在
// `<html>` 之后，再没有就放在 `<!doctype>` 之后（在 doctype 前面插脚本会让页面掉进怪异模式），都没有才是最前面。
// 按字节处理（用 latin1 解码只为了找下标），所以页面是什么 ASCII 兼容的编码都行。

/** 找到插入点之前最多缓冲这么多字节。 */
export const MAX_INJECT_BUFFER = 64 * 1024

const BOM = [0xef, 0xbb, 0xbf]

const latin1 = new TextDecoder('latin1')

function isSpace(code: number): boolean {
  return code === 0x20 || code === 0x09 || code === 0x0a || code === 0x0c || code === 0x0d
}

/** 在开始标签 `<name ...>` 里找结束的 `>` 的下标（跳过引号里的）。没找到返回 -1。 */
function tagEnd(text: string, from: number): number {
  let quote = ''
  for (let i = from; i < text.length; i++) {
    const c = text[i]!
    if (quote) {
      if (c === quote) quote = ''
    } else if (c === '"' || c === "'") quote = c
    else if (c === '>') return i
  }
  return -1
}

/**
 * 算插入点（字节下标）。返回 `undefined` 表示还要更多数据才能定；`final` 为真（流结束或缓冲满了）时一定给出答案。
 */
export function findInsertionPoint(bytes: Uint8Array, final: boolean): number | undefined {
  const text = latin1.decode(bytes)
  const n = text.length
  let i = BOM.every((b, k) => bytes[k] === b) ? 3 : 0
  // 没有 <head> / <html> 时的备选位置：doctype 之后，再不行就是文档开头（BOM 之后）
  let fallback = i
  const decide = (): number | undefined => (final ? fallback : undefined)
  for (;;) {
    while (i < n && isSpace(text.charCodeAt(i))) i++
    if (i >= n) return decide()
    if (text[i] !== '<') return fallback // 标签之前就有正文：不再往后找
    if (text.startsWith('<!--', i)) {
      const end = text.indexOf('-->', i + 4)
      if (end < 0) return decide()
      i = end + 3
      continue
    }
    if (text.startsWith('<?', i)) {
      const end = text.indexOf('?>', i + 2)
      if (end < 0) return decide()
      i = end + 2
      fallback = i
      continue
    }
    if (text.startsWith('<!', i)) {
      // doctype 或别的声明
      const end = text.indexOf('>', i)
      if (end < 0) return decide()
      i = end + 1
      fallback = i
      continue
    }
    const m = /^<([a-zA-Z][a-zA-Z0-9]*)/.exec(text.slice(i, i + 32))
    if (!m) return n - i === 1 && !final ? undefined : fallback // 末尾只有一个 `<`：下一块才知道它是什么
    // 标签名可能被缓冲区截断
    if (i + m[0].length >= n && !final) return undefined
    const name = m[1]!.toLowerCase()
    const end = tagEnd(text, i + m[0].length)
    if (end < 0) return decide()
    if (name === 'head') return end + 1
    if (name === 'html') {
      i = end + 1
      fallback = i
      continue
    }
    return fallback // 别的标签：这个文档的 <head> 是隐含的，放在 <html> / doctype 之后
  }
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length)
  out.set(a, 0)
  out.set(b, a.length)
  return out
}

/** 在第一个插入点把 `insert` 的字节塞进流里的 TransformStream。 */
export function createInjector(insert: Uint8Array): TransformStream<Uint8Array, Uint8Array> {
  let buffer: Uint8Array = new Uint8Array(0)
  let done = false
  const emit = (controller: TransformStreamDefaultController<Uint8Array>, final: boolean): void => {
    const at = findInsertionPoint(buffer, final || buffer.length >= MAX_INJECT_BUFFER)
    if (at === undefined) return
    const out = new Uint8Array(buffer.length + insert.length)
    out.set(buffer.subarray(0, at), 0)
    out.set(insert, at)
    out.set(buffer.subarray(at), at + insert.length)
    controller.enqueue(out)
    buffer = new Uint8Array(0)
    done = true
  }
  return new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      if (done) return controller.enqueue(chunk)
      buffer = concat(buffer, chunk)
      emit(controller, false)
    },
    flush(controller) {
      if (!done) emit(controller, true)
    },
  })
}

const ESCAPES: Record<string, string> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }

export function escapeAttribute(value: string): string {
  return value.replace(/[&<>"']/g, (c) => ESCAPES[c]!)
}

/** `<script src="/__anycon/shim.js" data-cookies="…"></script>`，`cookies` 是 `document.cookie` 形式的字符串。 */
export function shimTag(cookies: string): Uint8Array {
  return new TextEncoder().encode(`<script src="/__anycon/shim.js" data-cookies="${escapeAttribute(cookies)}"></script>`)
}
