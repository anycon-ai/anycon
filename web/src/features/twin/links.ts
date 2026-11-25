// Bot 连接（使用别的节点）的 RPC：添加是两步，先问被使用的节点 B 要短码，再把短码交给使用方 A。
// 短码是秘密：只作为局部变量经内存从第一步的结果传到第二步的请求，不进状态、不进日志、不进错误信息。
import type { TFunction } from 'i18next'
import type { NodeInfo, NodeTwinLinkCodeResult, TwinLink, TwinLinksResult } from '@/protocol'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { describeError } from '@/lib/errors'

/** 添加连接时哪一步失败了：`code` 是向 B 要短码，`link` 是 A 用短码配对。 */
export type TwinLinkStep = 'code' | 'link'
export class TwinLinkError extends Error {
  constructor(
    readonly step: TwinLinkStep,
    override readonly cause: unknown,
  ) {
    // 消息里只放步骤名，不带 RPC 的任何内容
    super(`twin link failed at step ${step}`)
    this.name = 'TwinLinkError'
  }
}

export async function listTwinLinks(nodeId: string): Promise<TwinLink[]> {
  return (await rpc().request<TwinLinksResult>(nodeId, 'twin.links', {})).links
}

/**
 * `twin.link` 的超时：daemon 要等 B 确认配对（最多 30 秒），hub 限速时还会按 `Retry-After` 等（最多 30 秒，daemon.md § 27.2），
 * 缺省的 30 秒会在 daemon 还在等的时候就报超时。
 */
const TWIN_LINK_TIMEOUT_MS = 75_000

/** 依次调 B 的 `node.twin_link_code` 和 A 的 `twin.link`；任何一步失败都抛 `TwinLinkError`，不重试（再要短码会作废旧的）。 */
export async function addTwinLink(usingNode: string, usedNode: string): Promise<TwinLink> {
  let code: string
  try {
    code = (await rpc().request<NodeTwinLinkCodeResult>(usedNode, 'node.twin_link_code', {})).code
  } catch (e) {
    throw new TwinLinkError('code', e)
  }
  try {
    return await rpc().request<TwinLink>(usingNode, 'twin.link', { code, node_id: usedNode }, TWIN_LINK_TIMEOUT_MS)
  } catch (e) {
    throw new TwinLinkError('link', e)
  }
}

export async function removeTwinLink(usingNode: string, usedNode: string): Promise<void> {
  await rpc().request(usingNode, 'twin.unlink', { node_id: usedNode })
}

/** 守卫：节点还不认识 `twin.links`（旧 daemon）时 `unsupported`，界面据此不显示这一块。 */
export const isUnsupported = (e: unknown): boolean => e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'unsupported'

const REASONS: Record<string, string> = {
  accept_links_off: 'acceptLinksOff',
  not_found: 'notFound',
  timeout: 'notAccepted',
  hub_changed: 'hubChanged',
  no_hub: 'noHub',
  node_mismatch: 'nodeMismatch',
  hub_unsupported: 'hubUnsupported',
  hub_rate_limited: 'hubRateLimited',
}

/** 把添加失败按 `data.reason` 翻成人话；不认识的原因落到按错误码的通用说明。 */
export function describeLinkError(e: unknown, t: TFunction): string {
  if (!(e instanceof TwinLinkError)) return describeError(e, t)
  const cause = e.cause
  if (cause instanceof RpcCallError && cause.kind === 'rpc') {
    const key = cause.reason ? REASONS[cause.reason] : undefined
    if (key) return t(`twinLinks.errors.${key}`)
    if (cause.code === 'busy' && (cause.reason === 'quota' || cause.reason === 'rate_limited')) return t('twinLinks.errors.pairBusy')
  }
  return `${t(e.step === 'code' ? 'twinLinks.errors.codeStep' : 'twinLinks.errors.linkStep')} ${describeError(cause, t)}`
}

/** 添加用的候选：这台设备配过对的别的节点；不能选的带原因（说明怎么办放在界面文案里）。 */
export type CandidateBlock = 'linked' | 'unsupported' | 'acceptOff' | 'offline'
export function candidateBlock(info: NodeInfo | undefined, online: boolean, linked: boolean): CandidateBlock | null {
  if (linked) return 'linked'
  if (info?.capabilities.twin !== true) return 'unsupported'
  if (info.twin_accept_links !== true) return 'acceptOff'
  if (!online) return 'offline'
  return null
}
/** 分组名不同（含一边没有）时，添加要多确认一次。 */
export const groupsDiffer = (a: string | null | undefined, b: string | null | undefined): boolean => (a ?? '') !== (b ?? '')
