import { useParams } from '@tanstack/react-router'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { resolveNodeParam, shortNodeId } from './ids'

/**
 * 登录后回到哪里：只认同源的绝对路径（`/pair#code=…` 这样带 search / hash 的完整地址也行），
 * 其它（`//evil.example`、`https://…`、`javascript:`）一律回首页。
 */
export function safeRedirect(href: string | undefined): string {
  return href && /^\/(?![/\\])/.test(href) ? href : '/'
}

/** 当前 URL 里的节点（短 id → 完整 id）。 */
export function useCurrentNode(): { param: string | undefined; nodeId: string | undefined; entry: NodeEntry | undefined; loaded: boolean } {
  const params = useParams({ strict: false }) as { node?: string }
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const loaded = useNodes((s) => s.loaded)
  const nodeId = params.node ? resolveNodeParam(params.node, order) : undefined
  return { param: params.node, nodeId, entry: nodeId ? byId[nodeId] : undefined, loaded }
}

/** 用于生成 URL 的节点参数。 */
export function useNodeParam(nodeId: string | undefined): string {
  const order = useNodes((s) => s.order)
  return nodeId ? shortNodeId(nodeId, order) : ''
}
