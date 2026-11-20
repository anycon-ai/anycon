// 「待处理」的数据入口：组件用这里的 hook（共用 `model.ts` 里的一份模型），不在 React 里的地方（应用角标）用 `model.ts` 的函数。
import { useSyncExternalStore } from 'react'
import { inboxNowCount, type InboxModel } from './derive'
import { getInboxSnapshot, subscribeInbox, type InboxSnapshot } from './model'

const snapshot = () => getInboxSnapshot()

/** 按节点过滤器算出的整个「待处理」。 */
export function useInbox(): InboxModel {
  return useSyncExternalStore(subscribeInbox, snapshot).model
}

/** 导航项上的数字（「现在」段的条数）。 */
export function useInboxNowCount(): number {
  return inboxNowCount(useInbox())
}

/** 没勾选的节点上「现在」段的条数：节点过滤按钮上的小点。 */
export function useInboxOutsideCount(): number {
  return useSyncExternalStore(subscribeInbox, snapshot).outside
}

/** 同上，带去处（按空间）：小点的可访问名称用。 */
export function useInboxOutsideWhere(): InboxSnapshot['outsideWhere'] {
  return useSyncExternalStore(subscribeInbox, snapshot).outsideWhere
}
