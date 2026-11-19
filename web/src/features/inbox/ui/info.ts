// 一个条目在行里和详情里要显示的文字：标题、所属会话 / 项目 / 节点、等了多久、节点是不是在线。
import { useTranslation } from 'react-i18next'
import { cleanHintText } from '@/lib/hint'
import { formatRelative } from '@/lib/format'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import type { InboxItem } from '../derive'

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
    case 'enroll':
      return { ...base, title: t('inbox.enrollTitle', { node: cleanHintText(nodeName) || nodeName }), since: t('inbox.requestedSince', { time }) }
  }
}
