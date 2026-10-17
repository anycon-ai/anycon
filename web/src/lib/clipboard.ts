/**
 * 写剪贴板。
 *
 * 局域网 http 打开的页面不是安全上下文，浏览器不给 `navigator.clipboard`；`writeText` 也可能被权限拒绝。
 * 这两种情况都退回到隐藏 textarea + `execCommand('copy')`：已废弃但各浏览器仍支持，且不要求安全上下文。
 * 退路必须在点击事件里同步走到，所以没有 `navigator.clipboard` 时直接同步复制，不先 await。
 */
export async function copyText(text: string): Promise<void> {
  if (navigator.clipboard) {
    try {
      await navigator.clipboard.writeText(text)
      return
    } catch (e) {
      if (!legacyCopy(text)) throw e
      return
    }
  }
  if (!legacyCopy(text)) throw new Error('clipboard unavailable')
}

function legacyCopy(text: string): boolean {
  if (typeof document.execCommand !== 'function') return false
  const active = document.activeElement instanceof HTMLElement ? document.activeElement : null
  // 对话框 / 抽屉会把焦点锁在自己里面，textarea 要放进去才能被选中
  const host = active?.closest('[role="dialog"]') ?? document.body
  const ta = document.createElement('textarea')
  ta.value = text
  ta.setAttribute('readonly', '')
  ta.style.position = 'fixed'
  ta.style.top = '0'
  ta.style.left = '0'
  ta.style.opacity = '0'
  ta.style.pointerEvents = 'none'
  host.appendChild(ta)
  ta.focus({ preventScroll: true })
  ta.select()
  ta.setSelectionRange(0, text.length)
  let ok = false
  try {
    ok = document.execCommand('copy')
  } catch {
    ok = false
  }
  ta.remove()
  active?.focus({ preventScroll: true })
  return ok
}
