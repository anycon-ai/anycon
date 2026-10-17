// 页面连着 hub 但在后台时的系统通知（spec/web.md § 7）：连着 hub 的浏览器收不到 Web Push（hub 只推给没有在线连接的设备），
// 改为收到 daemon 发来的 `notify` 信封；页面 hidden、通知权限已授予、**本浏览器开着推送**（有订阅）时，经 service worker 的
// `registration.showNotification` 弹一条和 push 同样的通知（同样的文案、同样的 `tag`，点击也由 service worker 的 `notificationclick` 处理）。
// 页面在前台时不弹：那是应用内的 toast / 横幅。没开推送也不弹：在设置里点了「关闭」、或退出登录时退订了，就是「这台浏览器不要通知」，
// 通知权限还在不算数——否则公用电脑上退出登录之后，下一个人还会看到你的会话标题。
import type { TFunction } from 'i18next'
import type { NotifyBody, NotifyCategory } from '@/protocol'
import { shortNodeId } from '@/lib/ids'
import { getRegistration } from '@/features/push/browser'
import { pushEnabledFor } from '@/features/push/prefsCache'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import i18n from '@/i18n'
import { pushText, type PushText } from './pushText'

const ICON = '/icon-192.png'

export interface NotificationTarget {
  /** 本浏览器的推送订阅（没有 = null）：开着推送的标志。 */
  pushManager: { getSubscription(): Promise<unknown> }
  showNotification(title: string, options: NotificationOptions & { renotify?: boolean }): Promise<void>
}

export interface SystemNotifyEnv {
  visibility(): DocumentVisibilityState
  permission(): NotificationPermission | 'unsupported'
  registration(): Promise<NotificationTarget | null>
  pushEnabled(category: NotifyCategory): Promise<boolean>
  t: TFunction
  /** 本地已知的节点名 / 会话标题 / 全部节点 id。会话不属于这个节点就当不认识（别拿别的节点的标题）。 */
  nodeName(nodeId: string): string | undefined
  sessionTitle(nodeId: string, sessionId: string): string | undefined
  nodeIds(): readonly string[]
}

export function defaultEnv(): SystemNotifyEnv {
  return {
    visibility: () => document.visibilityState,
    permission: () => (typeof Notification === 'undefined' ? 'unsupported' : Notification.permission),
    registration: () => ('serviceWorker' in navigator ? getRegistration(1500) : Promise.resolve(null)),
    pushEnabled: (category) => pushEnabledFor(category),
    t: i18n.t.bind(i18n),
    nodeName: (id) => useNodes.getState().byId[id]?.name,
    sessionTitle: (nodeId, sessionId) => {
      const s = useSessions.getState().list[sessionId]
      return s && s.node_id === nodeId ? s.title : undefined
    },
    nodeIds: () => useNodes.getState().order,
  }
}

async function hasSubscription(reg: NotificationTarget): Promise<boolean> {
  try {
    const sub = await reg.pushManager.getSubscription()
    return sub !== null && sub !== undefined
  } catch (e) {
    console.debug('anycon: could not read the push subscription; not notifying', e)
    return false
  }
}

/** `notify` 信封 → 通知文案（纯函数，和 service worker 里用的是同一个 `pushText`）。 */
export function textForNotify(env: Pick<SystemNotifyEnv, 't' | 'nodeName' | 'sessionTitle' | 'nodeIds'>, nodeId: string, body: NotifyBody): PushText {
  const sessionId = body.session_id ?? null
  return pushText(env.t, {
    category: body.category,
    node: { id: nodeId, name: env.nodeName(nodeId), param: shortNodeId(nodeId, env.nodeIds()) },
    session: sessionId ? { id: sessionId, title: env.sessionTitle(nodeId, sessionId) } : null,
  })
}

/**
 * 页面在后台、已授权、本浏览器开着推送、偏好里这个类别的推送开着，就弹一条。返回是否弹了。
 * 前台、没授权、没有 service worker、本浏览器没开推送（没有订阅）、偏好关了都不弹（不是错误）。
 */
export async function showSystemNotification(nodeId: string, body: NotifyBody, env: SystemNotifyEnv = defaultEnv()): Promise<boolean> {
  if (env.visibility() !== 'hidden') return false
  if (env.permission() !== 'granted') return false
  const reg = await env.registration()
  if (!reg) return false
  // 本浏览器没开推送（在设置里关了、或退出登录时退订了）：通知权限还在也不弹
  if (!(await hasSubscription(reg))) return false
  if (!(await env.pushEnabled(body.category))) return false
  // 订阅 / 偏好请求期间页面可能又回到前台了
  if (env.visibility() !== 'hidden') return false
  const text = textForNotify(env, nodeId, body)
  await reg.showNotification(text.title, {
    ...(text.body ? { body: text.body } : {}),
    tag: text.tag,
    renotify: true,
    icon: ICON,
    lang: i18n.resolvedLanguage ?? 'en',
    data: { url: text.url },
  })
  return true
}
