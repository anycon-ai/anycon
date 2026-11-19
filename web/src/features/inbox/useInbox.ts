// 「待处理」的数据入口：组件用 `useInbox()`（订阅 store），不在 React 里的地方（应用角标）用 `inboxNowCountFromStores()`（读一次）。
import { useMemo } from 'react'
import { selectedNodes, useNodeFilter } from '@/lib/nodeFilter'
import { useNow } from '@/lib/useNow'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { deriveInbox, inboxNowCount, type InboxModel } from './derive'

/** 订阅 store，按节点过滤器算出整个「待处理」。有接入请求在倒计时时每秒重算一次，否则每分钟（出错会话的 7 天期限）。 */
export function useInbox(): InboxModel {
  const { selected } = useNodeFilter()
  const sessions = useSessions((s) => s.list)
  const pending = useSessions((s) => s.pending)
  const enrollItems = useEnroll((s) => s.items)
  const enrollResolved = useEnroll((s) => s.resolved)
  const counting = Object.keys(enrollItems).length > 0
  const now = useNow(counting ? 1000 : 60_000)
  return useMemo(
    () => deriveInbox({ sessions, pending, enroll: { items: enrollItems, resolved: enrollResolved }, selected, now }),
    [sessions, pending, enrollItems, enrollResolved, selected, now],
  )
}

/** 导航项上的数字（「现在」段的条数）。 */
export function useInboxNowCount(): number {
  return inboxNowCount(useInbox())
}

/** 非 React 的调用方（应用角标、标签页标题）：读一次 store，口径和 `useInbox` 完全一样。 */
export function inboxNowCountFromStores(now: number = Date.now()): number {
  const nodes = useNodes.getState()
  const sessions = useSessions.getState()
  const enroll = useEnroll.getState()
  return inboxNowCount(
    deriveInbox({
      sessions: sessions.list,
      pending: sessions.pending,
      enroll: { items: enroll.items, resolved: enroll.resolved },
      selected: selectedNodes(nodes.order, usePrefs.getState().sidebarNodes),
      now,
    }),
  )
}
