// 配对 / 接入期间 hub 控制消息和 handshake / data 信封进的邮箱事件（client.ts 往里推，pair() / enroll() 按序取）。
import type { EnrollFailReason, PairFailReason } from '@/protocol'

export type PairEvent =
  | { t: 'matched'; peer: string; role: 'host' | 'joiner' }
  | { t: 'failed'; reason: PairFailReason }
  | { t: 'handshake'; body: Uint8Array }
  | { t: 'done'; peer: string; deviceId?: string | null; deviceToken?: string | null }
  /** 配对完成后 daemon 发来的 `data` 信封（预期是 `pair.ack`）。 */
  | { t: 'data'; from: string; body: Uint8Array }
  /** 账号内接入（ADR-36）：hub 建好了接入会话。 */
  | { t: 'enroll_started'; enrollId: string; node: string; expiresAtMs: number }
  /** 账号内接入：hub 结束了接入会话（或拒绝了 `enroll_begin`）。`enrollId` 为空 = 没能建起会话。 */
  | { t: 'enroll_failed'; enrollId: string | null; reason: EnrollFailReason }

export type PairDoneEvent = Extract<PairEvent, { t: 'done' }>
