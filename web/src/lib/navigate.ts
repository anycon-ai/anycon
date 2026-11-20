// 非 React 代码（通知处理、toast 的动作按钮）要跳转页面时用：路由器建好后在 main.tsx 里登记，没登记（测试）就整页跳。
import { isSafePath } from './safePath'

let navigator: ((to: string) => void) | null = null

export function setNavigator(fn: ((to: string) => void) | null): void {
  navigator = fn
}

/** 站内跳转（`to` 是站内的绝对路径，可以带 query；别的一律忽略，见 `isSafePath`）。 */
export function navigateTo(to: string): void {
  if (!isSafePath(to)) return
  if (navigator) navigator(to)
  else location.assign(to)
}
