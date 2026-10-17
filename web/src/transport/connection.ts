// 连接状态机（spec/web.md § 6.4）。纯函数：事件进、状态出，副作用（定时器、ws）由调用方做。
//
//   hub_disconnected → hub_connecting → hub_authed
//       → peer_offline                    节点不在线，可看缓存、可写草稿
//       → noise_handshaking → ready       正常
//       → noise_failed                    daemon 不认识我们的公钥：被吊销或重装，提示重新配对
import type { AuthFailReason } from '@/protocol'
import { CLOSE_CODE } from '@/lib/constants'

export type HubPhase = 'hub_disconnected' | 'hub_connecting' | 'hub_authed'

/** 不再自动重连的终态，需要用户操作。 */
export type HubHalt = null | 'superseded' | 'revoked' | 'invalid_token' | 'version' | 'quota'

export interface HubMachine {
  phase: HubPhase
  /** 连续失败次数（auth_ok 后清零）。 */
  attempt: number
  halt: HubHalt
  /** 下一次重连前等多久；null = 不需要（已连上或进入终态）。 */
  retryInMs: number | null
}

export type HubEvent =
  | { type: 'connect' }
  | { type: 'auth_ok' }
  /** 连上并稳定了一阵（STABLE_AFTER_MS）：清零退避。 */
  | { type: 'stable' }
  | { type: 'auth_fail'; reason: AuthFailReason }
  | { type: 'closed'; code: number }
  | { type: 'visible' }
  /** 页面在后台满 30 秒，主动断开（spec/web.md § 7）：不算失败，不退避，回到前台（`visible`）立即重连。 */
  | { type: 'suspend' }

export const STABLE_AFTER_MS = 15_000

export const initialHub: HubMachine = { phase: 'hub_disconnected', attempt: 0, halt: null, retryInMs: 0 }

/** 指数退避 1s → 30s，带 ±25% 抖动。 */
export function backoffDelay(attempt: number, rng: () => number = Math.random): number {
  const base = Math.min(30_000, 1000 * 2 ** Math.max(0, attempt))
  return Math.round(base * (0.75 + rng() * 0.5))
}

/** 收到 4002 restarting：1–3 秒抖动。 */
export function restartDelay(rng: () => number = Math.random): number {
  return Math.round(1000 + rng() * 2000)
}

export function hubReduce(m: HubMachine, e: HubEvent, rng: () => number = Math.random): HubMachine {
  switch (e.type) {
    case 'connect':
      return { ...m, phase: 'hub_connecting', halt: null, retryInMs: null }
    case 'auth_ok':
      // attempt 不立刻清零：认证后马上又断的抖动连接要继续退避，稳定 STABLE_AFTER_MS 后由 `stable` 清零。
      return { phase: 'hub_authed', attempt: m.attempt, halt: null, retryInMs: null }
    case 'stable':
      return m.phase === 'hub_authed' ? { ...m, attempt: 0 } : m
    case 'auth_fail': {
      const base = { ...m, phase: 'hub_disconnected' as const }
      switch (e.reason) {
        case 'revoked':
          return { ...base, halt: 'revoked', retryInMs: null }
        case 'invalid':
          return { ...base, halt: 'invalid_token', retryInMs: null }
        case 'version':
          return { ...base, halt: 'version', retryInMs: null }
        case 'quota':
          return { ...base, halt: 'quota', retryInMs: null }
        default:
          return { ...base, attempt: m.attempt + 1, retryInMs: backoffDelay(m.attempt, rng) }
      }
    }
    case 'closed': {
      const base = { ...m, phase: 'hub_disconnected' as const }
      if (m.halt && m.halt !== 'superseded') return { ...base, retryInMs: null }
      if (e.code === CLOSE_CODE.SUPERSEDED) return { ...base, halt: 'superseded', retryInMs: null }
      if (e.code === CLOSE_CODE.REVOKED) return { ...base, halt: 'revoked', retryInMs: null }
      if (e.code === CLOSE_CODE.RESTARTING) return { ...base, retryInMs: restartDelay(rng) }
      return { ...base, attempt: m.attempt + 1, retryInMs: backoffDelay(m.attempt, rng) }
    }
    case 'visible': {
      // 页面回到前台：断开状态立即重连，忽略退避；终态里只有「被别的标签页顶掉」允许自动夺回。
      if (m.phase !== 'hub_disconnected') return m
      if (m.halt && m.halt !== 'superseded') return m
      return { ...m, halt: null, attempt: 0, retryInMs: 0 }
    }
    case 'suspend':
      // 不是失败：失败计数清零。终态（被吊销、版本不符……）保持原样，不因为后台就解除。
      return { ...m, phase: 'hub_disconnected', attempt: 0, retryInMs: m.halt && m.halt !== 'superseded' ? null : 0 }
  }
}

// ---- 每个节点 ----

export type NoiseState = 'none' | 'handshaking' | 'ready' | 'failed'

export interface NodeMachine {
  peerOnline: boolean
  noise: NoiseState
  /** 当前这轮 peer_online 内握手超时的次数。 */
  attempts: number
}

export const initialNode: NodeMachine = { peerOnline: false, noise: 'none', attempts: 0 }

export type NodeEvent =
  | { type: 'hub_down' }
  | { type: 'peer_online' }
  | { type: 'peer_offline' }
  | { type: 'handshake_started' }
  | { type: 'handshake_ok' }
  /** 握手消息读不出来（密钥不符）：直接判定失败。 */
  | { type: 'handshake_failed' }
  /** 对端没回握手：daemon 不认识我们时就是这个表现，重试一次后判定失败。 */
  | { type: 'handshake_timeout' }
  /** 传输层解密失败等，需要重新握手。 */
  | { type: 'reset' }

export const MAX_HANDSHAKE_ATTEMPTS = 2

export function nodeReduce(n: NodeMachine, e: NodeEvent): NodeMachine {
  switch (e.type) {
    case 'hub_down':
      return initialNode
    case 'peer_online':
      return { peerOnline: true, noise: 'none', attempts: 0 }
    case 'peer_offline':
      return initialNode
    case 'handshake_started':
      return n.peerOnline ? { ...n, noise: 'handshaking' } : n
    case 'handshake_ok':
      return n.peerOnline ? { ...n, noise: 'ready', attempts: 0 } : n
    case 'handshake_failed':
      return { ...n, noise: 'failed' }
    case 'handshake_timeout': {
      const attempts = n.attempts + 1
      return attempts >= MAX_HANDSHAKE_ATTEMPTS ? { ...n, noise: 'failed', attempts } : { ...n, noise: 'none', attempts }
    }
    case 'reset':
      return n.peerOnline ? { ...n, noise: 'none' } : n
  }
}

export type NodeConnState = HubPhase | 'peer_offline' | 'noise_handshaking' | 'ready' | 'noise_failed'

export function nodeConnState(hub: HubPhase, n: NodeMachine): NodeConnState {
  if (hub !== 'hub_authed') return hub
  if (!n.peerOnline) return 'peer_offline'
  switch (n.noise) {
    case 'ready':
      return 'ready'
    case 'failed':
      return 'noise_failed'
    default:
      return 'noise_handshaking'
  }
}
