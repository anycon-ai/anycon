// 预览票据（ADR-39，docs/spec/web.md § 13.1）：向 daemon 要票据（`preview.open`）、向 hub 要令牌（`POST /api/preview/tokens`），
// 拼成预览页的启动参数（`PreviewLaunch`）和 URL。
//
// 票据密钥和令牌都在 URL 的 fragment 里（浏览器不把 fragment 发给 hub）：这个模块不写日志，拿到的东西只放在内存里
// （不进 IndexedDB、不进路由状态、不复制到剪贴板）。
import { requestPreviewToken } from '@/api/preview'
import { getIdentity } from '@/db'
import { toHex } from '@/lib/bytes'
import {
  encodePreviewLaunch,
  PREVIEW_ENTER_PATH,
  PREVIEW_LAUNCH_VERSION,
  previewOrigin,
  type PreviewCloseParams,
  type PreviewLaunch,
  type PreviewOpenParams,
  type PreviewOpenResult,
} from '@/protocol'
import { useNodes } from '@/stores/nodes'
import { rpc } from '@/transport/instance'

/** 剩余有效期不到这么久的票据不再复用，重新走一遍（spec § 13.1）。 */
export const REFRESH_MARGIN_MS = 10 * 60_000

/** 本地就做不下去的原因（还没发出任何请求）。 */
export type PreviewLocalCode = 'no_device_token' | 'no_node_key' | 'bad_template' | 'bad_label'

export class PreviewLocalError extends Error {
  constructor(readonly code: PreviewLocalCode) {
    super(`preview: ${code}`)
    this.name = 'PreviewLocalError'
  }
}

/** 一个（节点, 端口）的预览票据：除了 `path` 之外的启动参数，加上它的 origin。含密钥，别写日志。 */
export interface PreviewTicket {
  nodeId: string
  port: number
  label: string
  /** `https://<label>.<预览域名>`，没有结尾的斜杠。 */
  origin: string
  /** 票据和令牌里较早的过期时间。 */
  expiresAtMs: number
  launch: Omit<PreviewLaunch, 'path'>
}

const tickets = new Map<string, PreviewTicket>()
const inflight = new Map<string, Promise<PreviewTicket>>()
const keyOf = (nodeId: string, port: number): string => `${nodeId}:${port}`

/** 缓存里这个端口的票据，不管还剩多久。停止预览要用它的 label。 */
export function knownTicket(nodeId: string, port: number): PreviewTicket | undefined {
  return tickets.get(keyOf(nodeId, port))
}

/** 缓存里这个端口的票据，剩余有效期够长才返回（够长的才值得同步地直接用，比如点「在新标签页打开」时）。 */
export function usableTicket(nodeId: string, port: number, now = Date.now()): PreviewTicket | undefined {
  const cached = tickets.get(keyOf(nodeId, port))
  return cached && cached.expiresAtMs - now >= REFRESH_MARGIN_MS ? cached : undefined
}

/** 忘掉这个 label 的全部票据（预览被停止了）。 */
export function forgetLabel(nodeId: string, label: string): void {
  for (const [key, ticket] of tickets) if (ticket.nodeId === nodeId && ticket.label === label) tickets.delete(key)
}

/** 清空缓存（测试用）。 */
export function clearTickets(): void {
  tickets.clear()
  inflight.clear()
}

export interface EnsureTicketArgs {
  nodeId: string
  port: number
  sessionId: string
  /** `/api/config.preview_origin_template`。 */
  template: string
}

/**
 * 取这个（节点, 端口）的票据：缓存里有且剩余有效期够长就用它，否则 `preview.open` → 令牌请求。
 * 同一个（节点, 端口）同时发起的请求合并成一次（StrictMode 下 effect 会跑两遍，也不该签两张票）。
 */
export function ensureTicket(args: EnsureTicketArgs): Promise<PreviewTicket> {
  const usable = usableTicket(args.nodeId, args.port)
  if (usable) return Promise.resolve(usable)
  const key = keyOf(args.nodeId, args.port)
  let pending = inflight.get(key)
  if (!pending) {
    pending = requestTicket(args).finally(() => inflight.delete(key))
    inflight.set(key, pending)
  }
  return pending
}

async function requestTicket({ nodeId, port, sessionId, template }: EnsureTicketArgs): Promise<PreviewTicket> {
  // 先查本地条件：缺了就别去 daemon 签一张用不上的票据
  if (!template.includes('{label}')) throw new PreviewLocalError('bad_template')
  const nodePublic = useNodes.getState().byId[nodeId]?.publicKey
  if (!nodePublic) throw new PreviewLocalError('no_node_key')
  const deviceToken = (await getIdentity())?.device_token
  if (!deviceToken) throw new PreviewLocalError('no_device_token')

  const params: PreviewOpenParams = { port, session_id: sessionId }
  const opened = await rpc().request<PreviewOpenResult>(nodeId, 'preview.open', params)
  let origin: string
  try {
    origin = previewOrigin(template, opened.label)
  } catch {
    throw new PreviewLocalError('bad_label')
  }
  const token = await requestPreviewToken(deviceToken, nodeId, opened.label)
  const expiresAtMs = Math.min(opened.expires_at_ms, token.expires_at_ms)
  const ticket: PreviewTicket = {
    nodeId,
    port: opened.port,
    label: opened.label,
    origin,
    expiresAtMs,
    launch: {
      v: PREVIEW_LAUNCH_VERSION,
      node: nodeId,
      node_public: toHex(nodePublic),
      label: opened.label,
      port: opened.port,
      ticket_id: opened.ticket_id,
      psk: opened.psk,
      token: token.token,
      expires_at_ms: expiresAtMs,
    },
  }
  tickets.set(keyOf(nodeId, port), ticket)
  return ticket
}

/** `<origin>/__anycon/enter#<启动参数>`：`path` 是预览页打开后先导航到的路径（以 `/` 开头）。 */
export function launchUrl(ticket: PreviewTicket, path: string): string {
  return `${ticket.origin}${PREVIEW_ENTER_PATH}#${encodePreviewLaunch({ ...ticket.launch, path })}`
}

/** 停止预览：作废这个 label 的全部票据（别的设备上开着的预览也会断开），并忘掉本地缓存。 */
export async function closePreview(nodeId: string, label: string): Promise<void> {
  const params: PreviewCloseParams = { label }
  await rpc().request(nodeId, 'preview.close', params)
  forgetLabel(nodeId, label)
}

/**
 * 地址栏里的内容 → 预览路径（以单个 `/` 开头，可带查询串）。
 * 粘贴整条 URL（`http://localhost:5173/a?b=1`）时只取路径部分；`//x`、`/\x` 这类浏览器会当成协议相对 URL 的写法收成 `/x`
 * （URL 解析器把 `\` 当 `/`，还会吃掉制表符和换行）。
 */
export function normalizePreviewPath(input: string): string {
  let text = input.replace(/[\t\n\r]/g, '').trim()
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(text)) {
    try {
      const url = new URL(text)
      text = url.pathname + url.search + url.hash
    } catch {
      /* 不是合法 URL：当普通路径处理 */
    }
  }
  return `/${text.replace(/^[/\\]+/, '')}`
}
