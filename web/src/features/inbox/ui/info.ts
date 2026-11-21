// 一个条目在行里和详情里要显示的文字：标题、所属会话 / 项目 / 节点、等了多久、节点是不是在线。
import { useSyncExternalStore } from 'react'
import { useTranslation } from 'react-i18next'
import { cleanHintText } from '@/lib/hint'
import { cleanOneLine } from '@/features/todos/validate'
import { formatRelative } from '@/lib/format'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import type { InboxItem } from '../derive'

// 相对时间（「3 分钟前」）每分钟刷新一次：模型在条目没变时是同一个对象，不会因为时间过去而重渲染，所以这里自己走一个分钟的节拍
// （所有行共用一个定时器，没有行在显示时不起）。
const minuteListeners = new Set<() => void>()
let minuteTimer: ReturnType<typeof setInterval> | null = null
const subscribeMinute = (cb: () => void): (() => void) => {
  minuteListeners.add(cb)
  minuteTimer ??= setInterval(() => minuteListeners.forEach((l) => l()), 60_000)
  return () => {
    minuteListeners.delete(cb)
    if (minuteListeners.size === 0 && minuteTimer) {
      clearInterval(minuteTimer)
      minuteTimer = null
    }
  }
}
const minuteNow = (): number => Math.floor(Date.now() / 60_000)
/** 当前的分钟数：用它当 hook 的依赖，渲染相对时间的组件每分钟重渲染一次。 */
export function useMinute(): number {
  return useSyncExternalStore(subscribeMinute, minuteNow)
}

export interface ItemInfo {
  title: string
  /** 请求属于哪个会话（标题）；不是会话类的条目没有。 */
  sessionTitle?: string
  sessionId?: string
  project?: string
  nodeName: string
  nodeParam: string
  /** 节点现在连得上：连不上时这条是缓存，答复和归档都要等它回来。 */
  online: boolean
  /** 「提出 / 停下来 / 出错 N 分钟前」。 */
  since: string
}

export function useItemInfo(item: InboxItem): ItemInfo {
  const { t, i18n } = useTranslation()
  useMinute()
  const node = useNodes((s) => s.byId[item.nodeId])
  const order = useNodes((s) => s.order)
  const listed = useSessions((s) => ('sessionId' in item ? s.list[item.sessionId] : undefined))
  const nodeName = node?.name ?? item.nodeId.slice(0, 8)
  const time = formatRelative(item.sinceMs, i18n.language)
  const base = { nodeName, nodeParam: shortNodeId(item.nodeId, order), online: node?.conn === 'ready' }
  switch (item.kind) {
    case 'permission':
    case 'plan':
    case 'question':
      return { ...base, title: item.request.title, sessionTitle: listed?.title || item.sessionId, sessionId: item.sessionId, project: listed?.project ?? undefined, since: t('inbox.waitingSince', { time }) }
    case 'waiting':
    case 'follow_up':
    case 'error':
      return {
        ...base,
        title: item.session.title || item.session.id,
        sessionId: item.session.id,
        project: item.session.project ?? undefined,
        since: t(item.kind === 'waiting' ? 'inbox.waitingSince' : item.kind === 'error' || item.session.status === 'error' ? 'inbox.erroredSince' : 'inbox.stoppedSince', { time }),
      }
    case 'review':
      return { ...base, title: cleanOneLine(item.todo.title), project: item.todo.project ? cleanOneLine(item.todo.project) : undefined, since: t('inbox.reviewSince', { time }) }
    case 'later':
      return { ...base, title: cleanOneLine(item.todo.title), project: item.todo.project ? cleanOneLine(item.todo.project) : undefined, since: t('inbox.addedSince', { time }) }
    case 'enroll':
      return { ...base, title: t('inbox.enrollTitle', { node: cleanHintText(nodeName) || nodeName }), since: t('inbox.requestedSince', { time }) }
  }
}
