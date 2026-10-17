// `push` 事件（spec/web.md § 7）：载荷 → 本地缓存的节点名 / 会话标题 → 文案 → `showNotification`。
// 每条推送都要显示一条通知（`userVisibleOnly: true` 的约束，不显示浏览器会弹一条默认的「网站已在后台更新」），
// 所以载荷读不懂、缓存里没有名字、读库失败，都退到兜底文案，而不是什么都不做。
import type { TFunction } from 'i18next'
import { genericPushText, nodeParam, pushText, type PushText } from '@/features/sync/pushText'
import { resolveLanguage, type SwLanguage } from './lang'
import { readPushPayload, type PushPayload } from './payload'
import type { PushDirectory } from './directory'
import type { ShowNotificationOptions } from './types'

export const NOTIFICATION_ICON = '/icon-192.png'

export interface PushDeps {
  directory(nodeId: string | null, sessionId: string | null): Promise<PushDirectory>
  /** 页面记下的界面语言；没有就是 undefined。 */
  language(): Promise<string | undefined>
  navigatorLanguage(): string | undefined
  translator(lang: SwLanguage): TFunction
  show(title: string, options: ShowNotificationOptions): Promise<void>
}

/** 通知本身（标题、tag、点击后打开的路径）；纯函数，`handlePush` 和测试共用。 */
export function notificationFor(t: TFunction, payload: PushPayload | null, dir: PushDirectory): PushText {
  if (!payload) return genericPushText(t)
  return pushText(t, {
    category: payload.category,
    node: payload.node ? { id: payload.node, name: dir.nodeName, param: nodeParam(payload.node, dir.nodeIds) } : null,
    session: payload.session ? { id: payload.session, title: dir.sessionTitle } : null,
  })
}

export function showOptions(text: PushText, lang: string): ShowNotificationOptions {
  return {
    ...(text.body ? { body: text.body } : {}),
    tag: text.tag,
    icon: NOTIFICATION_ICON,
    renotify: true,
    lang,
    data: { url: text.url },
  }
}

export async function handlePush(data: { json(): unknown } | null, deps: PushDeps): Promise<void> {
  const payload = readPushPayload(data)
  let dir: PushDirectory = { nodeIds: [] }
  try {
    dir = await deps.directory(payload?.node ?? null, payload?.session ?? null)
  } catch {
    /* 没有缓存就用兜底文案 */
  }
  let stored: string | undefined
  try {
    stored = await deps.language()
  } catch {
    /* 跟浏览器语言 */
  }
  const lang = resolveLanguage(stored, deps.navigatorLanguage())
  const text = notificationFor(deps.translator(lang), payload, dir)
  await deps.show(text.title, showOptions(text, lang))
}
