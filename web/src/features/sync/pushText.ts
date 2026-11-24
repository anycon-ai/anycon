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

/** 推送载荷的类别：全部 `NotifyCategory` 加上「测试推送」。 */
export type PushCategory = NotifyCategory | 'test'

/** 会话相关的五个类别：点开去会话页；`permission_requested` 例外，去「待处理」里定位那一条（spec/web.md § 7）。 */
export const SESSION_CATEGORIES = ['permission_requested', 'turn_finished', 'error', 'session_idle', 'follow_up'] as const
export type SessionCategory = (typeof SESSION_CATEGORIES)[number]

function isSessionCategory(category: PushCategory): category is SessionCategory {
  return SESSION_CATEGORIES.some((known) => known === category)
}

const SESSION_TITLE_KEY: Record<SessionCategory, string> = {
  permission_requested: 'push.permissionRequested',
  turn_finished: 'push.turnFinished',
  error: 'push.error',
  session_idle: 'push.sessionIdle',
  follow_up: 'push.followUp',
}

/**
 * 点开「权限请求」通知落在哪：`/inbox`，带上节点和会话，页面定位这个会话最早的一条待答复请求并展开。推送载荷只有类别、节点和会话，
 * 没有请求 id（ADR-55 的限制），所以同一个会话同时有几条请求时，定位到的可能不是触发这条通知的那一条。
 */
export function inboxUrl(nodeParamValue: string, sessionId: string): string {
  return `/inbox?node=${encodeURIComponent(nodeParamValue)}&session=${encodeURIComponent(sessionId)}`
}

// daemon 的清单落点使用 todo_ 前缀；普通会话和空后缀不作清单解析。
export function notifyTodoId(sessionId?: string | null): string | null {
  return sessionId?.startsWith('todo_') && sessionId.length > 5 ? sessionId.slice(5) : null
}

// Bot 的问题在待处理里定位清单项。
export function twinInboxUrl(node: string, todoId?: string | null): string {
  return `/inbox?node=${encodeURIComponent(node)}${todoId ? `&todo=${encodeURIComponent(todoId)}` : ''}`
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
  node?: { id: string; name?: string | null; param?: string; /** 节点的分组（空间名）：缓存的 `node.info` 里没有、或旧缓存没这个字段就是没有，第二行就不带空间。 */ group?: string | null } | null
  /**
   * 会话相关类别的 `session`（不透明 id）和本地缓存里的标题。`todo`：这个会话是从哪一项清单派出的（缓存里的 `todo_id`）和那一项的标题；
   * `turn_finished` 有标题时文案改成「<标题> 做完了，等你验收」并落在验收页，没有标题（缓存里没有那一项）就照旧。
   */
  session?: { id: string; title?: string | null; todo?: { id: string; title?: string | null } | null } | null
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
  const nodeName = cleanHintText(node.name)
  const space = cleanHintText(typeof node.group === 'string' ? node.group.trim() : '', 40)
  const param = node.param ?? node.id.slice(0, 8)
  const todoId = notifyTodoId(session?.id)
  if (category === 'twin_question') return {
    title: t('push.twinQuestion', { title: cleanHintText(session?.title, 80) || t(todoId ? 'push.aTodo' : 'push.aSession') }),
    tag: todoId ? `twin:${node.id}:${todoId}` : session ? `session:${session.id}` : `${category}:${node.id}`,
    url: todoId ? twinInboxUrl(param, todoId) : session ? `/n/${param}/s/${encodeURIComponent(session.id)}` : `/n/${param}`,
    body: space ? t('push.onNodeSpace', { node: nodeName || t('push.aNode'), space }) : t('push.onNode', { node: nodeName || t('push.aNode') }),
  }
  if (category === 'follow_up' && todoId) return {
    title: t('push.followUp', { title: cleanHintText(session?.title, 80) || t('push.aTodo') }),
    tag: `todo:${node.id}:${todoId}`,
    url: `/todos/${param}/${encodeURIComponent(todoId)}`,
    body: space ? t('push.onNodeSpace', { node: nodeName || t('push.aNode'), space }) : t('push.onNode', { node: nodeName || t('push.aNode') }),
  }
  // 还没有专用文案的新类别照常显示通用通知，避免按会话类别索引失败。
  if (!isSessionCategory(category)) return genericPushText(t)
  const sessionTitle = cleanHintText(session?.title, 80)
  // 清单项派出的会话做完了一轮：「<项的标题> 做完了，等你验收」，点开落在那一项的验收页（spec/web.md § 4.12、§ 7）。
  // 项的标题是用户 / agent 写的文字，和别的名字一样清理；缓存里没有标题就不猜，走下面原来的文案
  const todoTitle = category === 'turn_finished' && session?.todo?.id ? cleanHintText(session.todo.title, 80) : ''
  if (session?.todo && todoTitle) {
    return {
      title: t('push.todoReview', { title: `“${todoTitle}”` }),
      body: space ? t('push.onNodeSpace', { node: nodeName || t('push.aNode'), space }) : t('push.onNode', { node: nodeName || t('push.aNode') }),
      tag: `todo:${node.id}:${session.todo.id}`,
      url: `/todos/${param}/${encodeURIComponent(session.todo.id)}/review`,
    }
  }
  return {
    title: t(SESSION_TITLE_KEY[category], { title: sessionTitle ? `“${sessionTitle}”` : t('push.aSession') }),
    body: space ? t('push.onNodeSpace', { node: nodeName || t('push.aNode'), space }) : t('push.onNode', { node: nodeName || t('push.aNode') }),
    tag: session ? `session:${session.id}` : `${category}:${node.id}`,
    url: session ? (category === 'permission_requested' ? inboxUrl(param, session.id) : `/n/${param}/s/${encodeURIComponent(session.id)}`) : `/n/${param}`,
  }
}

/** 载荷读不懂时的兜底：浏览器要求每条推送都显示一条通知（`userVisibleOnly`），不能静默吞掉。 */
export function genericPushText(t: TFunction): PushText {
  return { title: t('push.generic'), tag: 'anycon', url: '/' }
}
