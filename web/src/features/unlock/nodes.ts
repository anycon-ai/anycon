// 「账号下有哪些节点、这个浏览器解锁了哪些」的纯函数：`/` 的重定向、`/unlock` 列表、左栏节点切换器共用。
import type { ApiDevice } from '@/api/types'
import { shortNodeId } from '@/lib/ids'

/** 账号下的 daemon（hub 的设备列表里 `kind = daemon`；吊销的 hub 不返回）。 */
export function accountNodes(devices: readonly ApiDevice[] | undefined): ApiDevice[] {
  return (devices ?? []).filter((d) => d.kind === 'daemon')
}

/** 账号下、这个浏览器还没配对（IndexedDB 里没有）的节点：可以去 `/unlock` 的那些。 */
export function lockedNodes(devices: readonly ApiDevice[] | undefined, localIds: readonly string[]): ApiDevice[] {
  const local = new Set(localIds)
  return accountNodes(devices).filter((d) => !local.has(d.id))
}

/**
 * 这个浏览器里配对过、但账号下已经没有的节点（在别的设备上被移除 / 吊销了）：给它们一个「忘记」入口。
 * `fetchedAt` 是设备列表的拉取时间：比节点的配对时间早的列表不算数（刚配对的节点可能还不在缓存的列表里）。列表没拉到时什么都不算。
 * 只用来提示和给入口，不自动删：hub 说了不算，删不删由用户决定。
 */
export function removedNodes(devices: readonly ApiDevice[] | undefined, fetchedAt: number, local: readonly { id: string; pairedAt: number }[]): string[] {
  if (!devices) return []
  const account = new Set(accountNodes(devices).map((d) => d.id))
  return local.filter((n) => !account.has(n.id) && fetchedAt > n.pairedAt).map((n) => n.id)
}

/** URL 里的节点参数（前 8 位，冲突时全长）→ 账号下的节点。 */
export function findAccountNode(param: string, nodes: readonly ApiDevice[]): ApiDevice | undefined {
  const exact = nodes.find((d) => d.id === param)
  if (exact) return exact
  const hits = nodes.filter((d) => d.id.startsWith(param))
  return hits.length === 1 ? hits[0] : undefined
}

/** `/unlock/:node` 里用的节点参数。 */
export function unlockParam(node: ApiDevice, nodes: readonly ApiDevice[]): string {
  return shortNodeId(node.id, nodes.map((d) => d.id))
}

export type HomeTarget = { to: 'wait' } | { to: 'node'; node: string } | { to: 'unlock' } | { to: 'setup' }

/**
 * `/` 去哪（spec/web.md § 2）：这个浏览器有已配对的节点 → 上次打开的；没有，但账号下有节点 → `/unlock`；账号下也没有 → `/setup`。
 * 设备列表拉不到（hub 不可达）时回落到 `/setup`——和以前没有 `/unlock` 时一样，那里能手输短码。
 */
export function homeTarget(i: { nodesLoaded: boolean; localOrder: readonly string[]; lastNode: string | null; devices: readonly ApiDevice[] | undefined; devicesFailed: boolean }): HomeTarget {
  if (!i.nodesLoaded) return { to: 'wait' }
  if (i.localOrder.length > 0) return { to: 'node', node: i.lastNode && i.localOrder.includes(i.lastNode) ? i.lastNode : i.localOrder[0]! }
  if (i.devices === undefined) return i.devicesFailed ? { to: 'setup' } : { to: 'wait' }
  return accountNodes(i.devices).length > 0 ? { to: 'unlock' } : { to: 'setup' }
}
