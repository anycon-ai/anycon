// 会话上的用户标注：「待处理」（follow_up）和未读。都是从 `SessionSummary` 算出来的，不存额外状态
// （daemon 只存 `follow_up` 和 `read_seq` 两个字段，spec/web.md § 4.6.1）。
import type { SessionSummary } from '@/protocol'
import { isActiveStatus } from './group'

type MarkFields = Pick<SessionSummary, 'status' | 'archived' | 'follow_up' | 'read_seq' | 'last_seq'>

/** 标了「待处理」。归档的会话不算：归档就是「放下了」。 */
export function isFollowUp(s: Pick<SessionSummary, 'archived' | 'follow_up'>): boolean {
  return !!s.follow_up && !s.archived
}

/** 标了「待处理」、会话还在跑：显示成「完成后提醒我」。 */
export function isFollowUpArmed(s: MarkFields): boolean {
  return isFollowUp(s) && isActiveStatus(s.status)
}

/** 标了「待处理」、会话没在跑：该你处理了（左栏置顶、角标计数）。 */
export function isFollowUpReady(s: MarkFields): boolean {
  return isFollowUp(s) && !isActiveStatus(s.status)
}

/**
 * 未读：会话没在跑，并且 `last_seq` 超过已读水位。运行中不亮（流式输出时 `last_seq` 一直在涨）；归档的不亮。
 * 水位缺失（本地缓存是旧版本写的）当作已读，不然升级后所有旧会话一起亮。
 */
export function isUnread(s: MarkFields): boolean {
  return !s.archived && !isActiveStatus(s.status) && s.last_seq > (s.read_seq ?? s.last_seq)
}
