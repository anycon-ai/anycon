// 全局状态横幅：按优先级只显示一条（spec/web.md § 3.4）。纯函数，便于测试。
import type { HubHalt, HubPhase, NodeConnState } from '@/transport/connection'

export type BannerKind =
  | 'hub_reconnecting'
  | 'hub_superseded'
  | 'hub_revoked'
  | 'hub_version'
  | 'hub_quota'
  | 'node_offline'
  | 'enroll_request'
  | 'storage'
  | 'unsent'
  | 'notifications'
  | 'update'

export interface BannerInput {
  hubPhase: HubPhase
  /** 连续失败次数；第一次连接中（0）不算「断开」，别让每次打开页面都闪红条。 */
  hubAttempt?: number
  hubHalt: HubHalt
  /** 传输层是否已经启动过（没启动时不要闪「重连中」）。 */
  started: boolean
  /** 当前页面所属节点的连接状态；不在节点页面时为 null。 */
  nodeConn: NodeConnState | null
  unsent: number
  /** 写本地存储失败过：未发送的消息、缓存可能在关掉标签页后丢失。 */
  storageError?: boolean
  notificationsPrompt: boolean
  updateReady: boolean
  /** 有几个设备在请求接入某台节点、等这台设备批准（ADR-36）。 */
  enrollRequests?: number
}

export function pickBanner(i: BannerInput): BannerKind | null {
  if (i.started) {
    if (i.hubHalt === 'superseded') return 'hub_superseded'
    if (i.hubHalt === 'revoked' || i.hubHalt === 'invalid_token') return 'hub_revoked'
    if (i.hubHalt === 'version') return 'hub_version'
    if (i.hubHalt === 'quota') return 'hub_quota'
    if (i.hubPhase !== 'hub_authed' && !(i.hubPhase === 'hub_connecting' && (i.hubAttempt ?? 1) === 0)) return 'hub_reconnecting'
  }
  if (i.nodeConn === 'peer_offline') return 'node_offline'
  if ((i.enrollRequests ?? 0) > 0) return 'enroll_request'
  if (i.storageError) return 'storage'
  if (i.unsent > 0) return 'unsent'
  if (i.notificationsPrompt) return 'notifications'
  if (i.updateReady) return 'update'
  return null
}
