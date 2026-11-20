// 「记一项」的输入条状态（spec/web.md § 14.5）：所有入口（手机底栏「＋」、桌面 `C`、侧栏「＋」、命令面板、会话页、分享、主屏快捷方式）
// 都落到同一个输入条，这里只记它开没开、里面的文字。文字只在内存里（草稿不落盘）。
import { create } from 'zustand'

interface CaptureState {
  open: boolean
  text: string
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
  openCapture: (opts) => set((s) => ({ open: true, focusTick: s.focusTick + 1, ...(opts?.text !== undefined ? { text: opts.text } : {}) })),
  close: () => set({ open: false }),
  setText: (text) => set({ text }),
}))

/** 把一段话（选中的文字、一条消息）变成记一项的预填：第一行（截到标题能放下的长度）是标题，原文和回到那里的链接在正文里。 */
export function captureFromQuote(quote: string, link?: string): string {
  const clean = quote.replace(/\r\n?/g, '\n').trim()
  const first = (clean.split('\n').find((l) => l.trim() !== '') ?? '').trim()
  const cps = [...first]
  const title = cps.length > 120 ? `${cps.slice(0, 120).join('')}…` : first
  const body = [clean, link].filter(Boolean).join('\n\n')
  return title === clean && !link ? title : `${title}\n${body}`
}
