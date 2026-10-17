// ⌘⇧A / ⌘⇧D：允许 / 拒绝当前会话最近一条 tool 类待审批（spec/web.md § 9）。
//
// 这是最容易误批准东西的快捷键，所以有几条硬规则：
//   - 焦点在输入框 / 文本域 / contenteditable 里不触发（正在打字，批准的可能是屏幕外的请求）；
//   - 只处理 tool 类请求（mode_change / plan / question 要看过再点）；
//   - 「允许」只会选 allow_once，「拒绝」只会选 reject_once——没有这种选项就什么都不做，绝不悄悄升级成 allow_always / reject_always；
//   - 请求的卡片不在可见区域时，这一下只滚到它那里让你看清楚，不答复；
//   - 按住不放（`e.repeat`）只算第一下。
import type { PermissionRespondParams } from '@/protocol'
import type { PermissionBlock } from './reducer'

export function isTypingTarget(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null
  if (!el) return false
  return el.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName)
}

export type ApproveAction = 'allow' | 'deny'

export function shortcutTarget(pending: PermissionBlock[], action: ApproveAction): { block: PermissionBlock; optionId: string } | null {
  const block = pending.filter((b) => b.kind === 'tool' && !b.resolved).at(-1)
  if (!block) return null
  const wanted = action === 'allow' ? 'allow_once' : 'reject_once'
  const option = block.options.find((o) => o.kind === wanted)
  return option ? { block, optionId: option.id } : null
}

export interface ApproveShortcutDeps {
  pending: PermissionBlock[]
  respond: (params: PermissionRespondParams) => void
  /** 那张卡片现在是否（至少部分）在可见区域里。 */
  isVisible: (requestId: string) => boolean
  /** 把那张卡片滚到视野中央，并提示用户先看清楚。 */
  reveal: (requestId: string) => void
}

/** 处理 keydown。返回 true 表示这个按键被消费了（已 preventDefault）。 */
export function handleApproveShortcut(e: Pick<KeyboardEvent, 'key' | 'metaKey' | 'ctrlKey' | 'shiftKey' | 'target' | 'preventDefault'> & { repeat?: boolean }, deps: ApproveShortcutDeps): boolean {
  const key = e.key.toLowerCase()
  if (!(e.metaKey || e.ctrlKey) || !e.shiftKey || (key !== 'a' && key !== 'd')) return false
  if (isTypingTarget(e.target)) return false
  const target = shortcutTarget(deps.pending, key === 'a' ? 'allow' : 'deny')
  if (!target) return false
  e.preventDefault()
  // 按住不放会不停地重复 keydown：只认第一下，否则一次长按能批准好几条请求
  if (e.repeat) return true
  if (!deps.isVisible(target.block.requestId)) {
    deps.reveal(target.block.requestId)
    return true
  }
  deps.respond({ request_id: target.block.requestId, option_id: target.optionId })
  return true
}
