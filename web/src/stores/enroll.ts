// 别的设备发来的接入请求（ADR-36）：`node.enroll_requested` / `node.enroll_resolved` / `node.enroll_list` 汇总在这里，
// 横幅（§ 3.4）、批准对话框、节点设置的「接入」标签共用。
// **只放请求的元数据**。批准后 daemon 给的 6 位码只在批准对话框的组件状态里，不进这个 store、不进 IndexedDB。
import { create } from 'zustand'
import type { EnrollInfo, EnrollResult } from '@/protocol'
import { localExpiry } from '@/lib/expiry'

export interface EnrollItem {
  nodeId: string
  info: EnrollInfo
  /** 到期时刻（本机时钟）：`info.expires_at_ms` 是 daemon 的时钟，不直接用。 */
  expiresAt: number
  receivedAt: number
  /** 请求发起的时刻（本机时钟），「2 分钟前」用。daemon 的 `created_at_ms` 对不上本机时钟时按收到的时刻算。 */
  createdAt: number
}

export interface EnrollResolution {
  result: EnrollResult
  /** 处理它的设备（daemon 自己判定的为空）。 */
  byDevice: string | null
  at: number
}

export const enrollKey = (nodeId: string, enrollId: string): string => `${nodeId}:${enrollId}`

/** 请求的发起时刻：daemon 的时钟和本机差不多（早不过一个有效期、不晚于现在一分钟）就用它，否则按收到的时刻。 */
function createdLocal(createdAtMs: number, now: number): number {
  return createdAtMs <= now + 60_000 && createdAtMs >= now - 6 * 60_000 ? Math.min(createdAtMs, now) : now
}

/** 过期后留着这么久再清掉（对话框里还要显示「已过期」）。 */
const KEEP_AFTER_EXPIRY_MS = 10 * 60_000

const STATUSES: ReadonlySet<string> = new Set(['waiting_hello', 'wait_approval', 'ready'])

/** 通知 / RPC 结果里的 `EnrollInfo` 来自 daemon：最基本的形状检查，缺字段的丢掉，不让一条坏数据弄坏界面。 */
export function parseEnrollInfo(v: unknown): EnrollInfo | null {
  if (typeof v !== 'object' || v === null) return null
  const o = v as Record<string, unknown>
  if (typeof o.enroll_id !== 'string' || o.enroll_id === '') return null
  if (typeof o.expires_at_ms !== 'number' || typeof o.created_at_ms !== 'number') return null
  if (typeof o.status !== 'string' || !STATUSES.has(o.status)) return null
  return v as EnrollInfo
}

interface EnrollState {
  items: Record<string, EnrollItem>
  resolved: Record<string, EnrollResolution>
  /** 批准对话框正在看的请求。 */
  dialog: { nodeId: string; enrollId: string } | null
  scannerOpen: boolean
  /** `node.security_event` 的计数：节点设置的「接入」标签据此重新读状态。 */
  securityTick: Record<string, number>
  upsert: (nodeId: string, info: EnrollInfo, now?: number) => void
  /** `node.enroll_list` 的结果：这个节点上进行中的请求以它为准（重连后补齐，也清掉离线期间已经结束的）。 */
  replaceNode: (nodeId: string, infos: EnrollInfo[], now?: number) => void
  resolve: (nodeId: string, enrollId: string, result: EnrollResult, byDevice: string | null, now?: number) => void
  remove: (nodeId: string, enrollId: string) => void
  openDialog: (nodeId: string, enrollId: string) => void
  closeDialog: () => void
  openScanner: () => void
  closeScanner: () => void
  bumpSecurity: (nodeId: string) => void
}

function prune(s: Pick<EnrollState, 'items' | 'resolved' | 'dialog'>, now: number): Pick<EnrollState, 'items' | 'resolved'> {
  const items: Record<string, EnrollItem> = {}
  const open = s.dialog ? enrollKey(s.dialog.nodeId, s.dialog.enrollId) : null
  for (const [k, it] of Object.entries(s.items)) if (k === open || it.expiresAt + KEEP_AFTER_EXPIRY_MS > now) items[k] = it
  const resolved: Record<string, EnrollResolution> = {}
  for (const [k, r] of Object.entries(s.resolved)) if (k === open || (k in items && r.at + KEEP_AFTER_EXPIRY_MS > now)) resolved[k] = r
  return { items, resolved }
}

export const useEnroll = create<EnrollState>()((set) => ({
  items: {},
  resolved: {},
  dialog: null,
  scannerOpen: false,
  securityTick: {},
  upsert: (nodeId, info, now = Date.now()) =>
    set((s) => {
      const key = enrollKey(nodeId, info.enroll_id)
      const received = s.items[key]?.receivedAt ?? now
      const next = { ...s, items: { ...s.items, [key]: { nodeId, info, expiresAt: localExpiry(info.expires_at_ms, now), receivedAt: received, createdAt: createdLocal(info.created_at_ms, received) } } }
      return prune(next, now)
    }),
  replaceNode: (nodeId, infos, now = Date.now()) =>
    set((s) => {
      const items: Record<string, EnrollItem> = {}
      for (const [k, it] of Object.entries(s.items)) if (it.nodeId !== nodeId) items[k] = it
      for (const info of infos) {
        const key = enrollKey(nodeId, info.enroll_id)
        const received = s.items[key]?.receivedAt ?? now
        items[key] = { nodeId, info, expiresAt: localExpiry(info.expires_at_ms, now), receivedAt: received, createdAt: createdLocal(info.created_at_ms, received) }
      }
      return prune({ ...s, items }, now)
    }),
  resolve: (nodeId, enrollId, result, byDevice, now = Date.now()) =>
    set((s) => {
      const key = enrollKey(nodeId, enrollId)
      const next = { ...s, resolved: { ...s.resolved, [key]: { result, byDevice, at: now } } }
      return prune(next, now)
    }),
  remove: (nodeId, enrollId) =>
    set((s) => {
      const key = enrollKey(nodeId, enrollId)
      const { [key]: _item, ...items } = s.items
      const { [key]: _res, ...resolved } = s.resolved
      return { items, resolved }
    }),
  openDialog: (nodeId, enrollId) => set({ dialog: { nodeId, enrollId } }),
  closeDialog: () =>
    set((s) => {
      // 看完就清：已经处理掉的请求不再留着
      const open = s.dialog ? enrollKey(s.dialog.nodeId, s.dialog.enrollId) : null
      if (!open || !s.resolved[open]) return { dialog: null }
      const { [open]: _item, ...items } = s.items
      const { [open]: _res, ...resolved } = s.resolved
      return { dialog: null, items, resolved }
    }),
  openScanner: () => set({ scannerOpen: true }),
  closeScanner: () => set({ scannerOpen: false }),
  bumpSecurity: (nodeId) => set((s) => ({ securityTick: { ...s.securityTick, [nodeId]: (s.securityTick[nodeId] ?? 0) + 1 } })),
}))

/** 等着一台已配对设备批准的请求（`code` 且请求批准）：横幅、批准对话框、「接入」标签的待批准列表都只看这些。 */
export function isPendingApproval(it: EnrollItem, resolved: Record<string, EnrollResolution>, now: number): boolean {
  return it.info.method === 'code' && it.info.approval === true && it.info.status === 'wait_approval' && !resolved[enrollKey(it.nodeId, it.info.enroll_id)] && it.expiresAt > now
}

/** `scan` 请求：只能靠扫码批准（需要二维码里的密钥），列表里只能拒绝。 */
export function isPendingScan(it: EnrollItem, resolved: Record<string, EnrollResolution>, now: number): boolean {
  return it.info.method === 'scan' && it.info.status === 'wait_approval' && !resolved[enrollKey(it.nodeId, it.info.enroll_id)] && it.expiresAt > now
}

/** 最早到的在前（先来先批）。 */
export function pendingApprovals(state: Pick<EnrollState, 'items' | 'resolved'>, now: number, nodeId?: string): EnrollItem[] {
  return Object.values(state.items)
    .filter((it) => (nodeId === undefined || it.nodeId === nodeId) && isPendingApproval(it, state.resolved, now))
    .sort((a, b) => a.receivedAt - b.receivedAt)
}
