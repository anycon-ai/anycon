// 「记一项」的输入条状态（spec/web.md § 14.5）：所有入口（手机底栏「＋」、桌面 `C`、侧栏「＋」、命令面板、会话页、分享、主屏快捷方式）
// 都落到同一个输入条，这里只记它开没开、里面的文字。文字只在内存里（草稿不落盘）。
import { create } from 'zustand'
import i18n from '@/i18n'

interface CaptureState {
  open: boolean
  text: string
  /** `/capture` 页面自己带着同一个表单（内联）：这时 `openCapture` 只把焦点拉回表单，不再开条 / 弹层（不能有两个表单绑着同一段文字）。 */
  inline: boolean
  setInline: (v: boolean) => void
  /** 每次 `openCapture` 加一：输入条据此重新聚焦（已经开着时再按一次也要把焦点拉回来）。 */
  focusTick: number
  /** `text` 给了就把输入框换成它（入口带来的预填：选中的文字、分享的内容、命令面板里输入的字）；不给就保留正在写的。 */
  openCapture: (opts?: { text?: string }) => void
  close: () => void
  setText: (text: string) => void
}

export const useCapture = create<CaptureState>()((set) => ({
  open: false,
  text: '',
  focusTick: 0,
  inline: false,
  setInline: (inline) => set({ inline, ...(inline ? { open: false } : {}) }),
  openCapture: (opts) => set((s) => ({ open: !s.inline, focusTick: s.focusTick + 1, ...(opts?.text !== undefined ? { text: opts.text } : {}) })),
  close: () => set({ open: false }),
  setText: (text) => set({ text }),
}))

/** 去掉标题里的 Markdown 记号：标题是一行纯文字，不能以 `##`、`>`、列表符、代码围栏开头，链接只留「文字 (地址)」。 */
export function plainTitle(line: string): string {
  return line
    .replace(/!?\[([^\]]*)\]\(([^)\s]*)[^)]*\)/g, (_m, text: string, url: string) => (text && url ? `${text} (${url})` : text || url))
    .replace(/^\s*(?:`{3,}|~{3,})\S*\s*$/, '')
    .replace(/^\s*(?:#{1,6}\s+|>+\s*|[-*+]\s+|\d+[.)]\s+)+/, '')
    .replace(/[*_~`]+/g, '')
    .replace(/\s+/g, ' ')
    .trim()
}

/**
 * GFM 会把裸的 `https://…`、`www.…`、邮箱地址自动变成能点的链接：在触发它们的地方插一个零宽空格（协议名和 `:` 之间、`www` 和 `.` 之间、`@` 前面）。
 * 看起来一样，复制出来也能用，只是渲染时不再被认成链接。
 */
export function breakAutolinks(line: string): string {
  return line
    .replace(/\b(https?|ftps?|mailto|xmpp)(?=:)/gi, '$&\u200b')
    .replace(/\bwww(?=\.)/gi, '$&\u200b')
    .replace(/@/g, '\u200b@')
}

/** 引用的原文放进正文：普通文字做成引用块（`[` `]` `<` `\` 转义，里面的链接不会变成能点的链接）；本身带代码围栏的用更长的围栏包住。 */
export function quoteBlock(clean: string): string {
  if (/^\s*(?:`{3,}|~{3,})/m.test(clean)) {
    const longest = Math.max(2, ...[...clean.matchAll(/`+/g)].map((m) => m[0].length))
    const fence = '`'.repeat(longest + 1)
    return `${fence}\n${clean}\n${fence}`
  }
  return clean
    .split('\n')
    .map((l) => `> ${breakAutolinks(l.replace(/[\\[\]<]/g, (c) => `\\${c}`))}`.trimEnd())
    .join('\n')
}

/**
 * 把一段话（选中的文字、一条消息）变成记一项的预填。原文来自会话（agent 写的、可能带不可信的 Markdown），而记下来的项是用户自己的：
 * 所以原文放在「来自会话」那一行下面的引用块里（链接不可点），标题是去掉 Markdown 记号的第一行；`link` 是回到那条消息的站内路径。
 */
export function captureFromQuote(quote: string, link?: string): string {
  const clean = quote.replace(/\r\n?/g, '\n').trim()
  const lines = clean.split('\n').map(plainTitle).filter((l) => l !== '')
  const first = lines.find((l) => /[\p{L}\p{N}]/u.test(l)) ?? lines[0] ?? ''
  const cps = [...first]
  const title = cps.length > 120 ? `${cps.slice(0, 120).join('')}…` : first || i18n.t('todos.quoteTitleFallback')
  const body = [i18n.t('todos.quotedFrom'), quoteBlock(clean), link ? `[${i18n.t('todos.openMessage')}](${link})` : ''].filter(Boolean).join('\n\n')
  return `${title}\n${body}`
}
