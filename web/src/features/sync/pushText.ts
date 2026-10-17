// 推送 / 系统通知的文案（spec/web.md § 7，ADR-37）：hub 的推送载荷只有 `{ category, node, session }`，没有任何正文，
// service worker（`src/sw/`）和页面在后台时的系统通知（`./systemNotify.ts`）都用这里的纯函数、拿本地缓存的节点名 / 会话标题组装。
// 接入相关的两个类别没有 `session`，点开去该节点设置的「接入」标签。这个文件不碰 DOM、不依赖 React，service worker 也会打包它。
import type { TFunction } from 'i18next'
import type { NotifyCategory } from '@/protocol'
import { cleanHintText } from '@/lib/hint'

export interface PushText {
  title: string
  /** 第二行：节点名（会话类才有）。 */
  body?: string
  /** 同一个 tag 的多条通知合并成一条。 */
  tag: string
  /** 点击后打开的站内路径。 */
  url: string
}

/** 推送载荷的类别：六个 `NotifyCategory` 加上「测试推送」。 */
export type PushCategory = NotifyCategory | 'test'

/** 会话相关的四个类别：点开去会话页。 */
export const SESSION_CATEGORIES = ['permission_requested', 'turn_finished', 'error', 'session_idle'] as const
export type SessionCategory = (typeof SESSION_CATEGORIES)[number]

const SESSION_TITLE_KEY: Record<SessionCategory, string> = {
  permission_requested: 'push.permissionRequested',
  turn_finished: 'push.turnFinished',
  error: 'push.error',
  session_idle: 'push.sessionIdle',
}

/** 接入相关的两个类别：`enroll_requested` 和 `security`。 */
export function enrollPushText(t: TFunction, category: Extract<NotifyCategory, 'enroll_requested' | 'security'>, node: { id: string; name?: string | null; param?: string }): PushText {
  const name = cleanHintText(node.name) || t('push.aNode')
  const param = node.param ?? node.id.slice(0, 8)
  return {
    title: category === 'enroll_requested' ? t('push.enrollRequested', { node: name }) : t('push.security', { node: name }),
    tag: `${category}:${node.id}`,
    // enroll_requested 时「接入」标签自动弹出待批准请求的对话框
    url: `/n/${param}/settings?tab=enroll${category === 'enroll_requested' ? '&review=1' : ''}`,
  }
}

export interface PushTextInput {
  category: PushCategory
  /** 节点（daemon）的设备 id；`test` 没有。 */
  node?: { id: string; name?: string | null; param?: string } | null
  /** 会话相关类别的 `session`（不透明 id）和本地缓存里的标题。 */
  session?: { id: string; title?: string | null } | null
}

/** 路由里节点参数的写法：前 8 位，冲突时回退全长（和 `shortNodeId` 一致，这里不依赖它，免得 service worker 多打包东西）。 */
export function nodeParam(id: string, all: readonly string[] = []): string {
  const short = id.slice(0, 8)
  return all.some((o) => o !== id && o.startsWith(short)) ? id : short
}

/**
 * 载荷 → 通知文案。缓存里没有名字时用「一个会话」/「一台节点」兜底；认不出节点（载荷没带）时点开只回首页。
 * 会话类按会话合并（`tag` 同一会话只留最新一条），接入类按节点合并。
 */
export function pushText(t: TFunction, input: PushTextInput): PushText {
  const { category, node, session } = input
  if (category === 'test') return { title: t('push.test'), body: t('push.testBody'), tag: 'test', url: '/settings/notifications' }
  if (!node) return { title: t('push.generic'), tag: category, url: '/' }
  if (category === 'enroll_requested' || category === 'security') return enrollPushText(t, category, node)
  const param = node.param ?? node.id.slice(0, 8)
  const nodeName = cleanHintText(node.name)
  const sessionTitle = cleanHintText(session?.title, 80)
  return {
    title: t(SESSION_TITLE_KEY[category], { title: sessionTitle ? `“${sessionTitle}”` : t('push.aSession') }),
    body: t('push.onNode', { node: nodeName || t('push.aNode') }),
    tag: session ? `session:${session.id}` : `${category}:${node.id}`,
    url: session ? `/n/${param}/s/${encodeURIComponent(session.id)}` : `/n/${param}`,
  }
}

/** 载荷读不懂时的兜底：浏览器要求每条推送都显示一条通知（`userVisibleOnly`），不能静默吞掉。 */
export function genericPushText(t: TFunction): PushText {
  return { title: t('push.generic'), tag: 'anycon', url: '/' }
}
