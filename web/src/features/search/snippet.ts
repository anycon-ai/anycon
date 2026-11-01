// 搜索命中的片段（`session.search` 的 `SearchHit.snippet`，docs/protocol.md § 5.4）。
//
// daemon 给的是「转义过的文字 + 字面的 `<mark>` / `</mark>`」：会话文字里原有的 `<` `>` `&` 已经转成 `&lt;` `&gt;` `&amp;`。
// 片段来自会话内容（agent 说的话、用户贴的东西），所以这里**不经过 HTML 解析器**：按字面 `<mark>` / `</mark>` 切开，
// 每一段里把这三个实体各还原一次，交给 React 当文本节点显示。除了这两个标签，其余一律是纯文本——
// 哪怕片段里出现没转义的 `<script>`（不该发生，但对端不可信），也只是显示成这几个字。
import { stripUnsafe } from '@/lib/hint'

export interface SnippetPart {
  text: string
  /** 命中词（在 `<mark>` 里）。 */
  mark: boolean
}

/** 一条命中最多显示多少个字符：daemon 的片段只有十几个词，但词可以很长（base64、压缩过的代码）。 */
export const SNIPPET_MAX_CHARS = 300

const MARK_TAG = /<\/?mark>/g
const ENTITY = /&(amp|lt|gt);/g
const ENTITY_CHAR = { amp: '&', lt: '<', gt: '>' } as const

/** 一遍还原，不会把还原出来的 `&` 再当成实体的开头：`&amp;lt;` → `&lt;`，不是 `<`。 */
function decode(s: string): string {
  return s.replace(ENTITY, (_m, name: keyof typeof ENTITY_CHAR) => ENTITY_CHAR[name])
}

/**
 * 把 daemon 的片段拆成「文字 / 命中词」交替的几段。
 * 标签没配对（比如只有 `</mark>`）不报错：多余的结束标签忽略，没收口的开始标签管到片段末尾。
 * 控制字符和双向控制符先换成空格（会话里的文字不能改写周围界面文字的顺序），超过 `maxChars` 个字符的部分截掉、补 `…`。
 */
export function parseSnippet(snippet: string, maxChars: number = SNIPPET_MAX_CHARS): SnippetPart[] {
  const parts: SnippetPart[] = []
  let budget = maxChars
  let mark = false
  let from = 0
  const push = (raw: string): void => {
    if (budget <= 0 || raw === '') return
    const chars = [...decode(stripUnsafe(raw))]
    if (chars.length === 0) return
    const cut = chars.length > budget
    const text = (cut ? chars.slice(0, budget) : chars).join('') + (cut ? '…' : '')
    budget = cut ? 0 : budget - chars.length
    const last = parts[parts.length - 1]
    if (last && last.mark === mark) last.text += text
    else parts.push({ text, mark })
  }
  for (const m of snippet.matchAll(MARK_TAG)) {
    push(snippet.slice(from, m.index))
    mark = m[0] === '<mark>'
    from = m.index + m[0].length
  }
  push(snippet.slice(from))
  return parts
}
