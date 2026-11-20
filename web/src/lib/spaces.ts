// 空间（spec/web.md § 4.13、docs/spec/daemon.md § 21.7）：分组名相同的已配对节点是一个空间。分组名是任何已配对设备都能写到节点上的文字，
// 一律当作不可信的显示文本：身份用去掉首尾空白之后的**原文**（区分大小写），显示用清理过的文字；两个原文不同的名字清理之后相同，
// 显示时加序号区分，身份不会撞。纯函数，不依赖 React。
import type { NodeInfo } from '@/protocol'
import { cleanHintText } from './hint'

/** 分组名最多这么多个码点（和 daemon 的 `hub.group` 一致）。 */
export const GROUP_MAX = 40

/** 一个节点的分组（身份）：去掉首尾空白的原文；没有（旧 daemon、没设、还没拿到过 `node.info`）是空串。 */
export function groupOf(info: Pick<NodeInfo, 'group'> | null | undefined): string {
  return typeof info?.group === 'string' ? info.group.trim() : ''
}

export interface Space {
  /** 身份：去掉首尾空白的分组名原文。 */
  key: string
  /** 显示用：清理过（没有控制字符 / 双向控制符），重名时带序号。 */
  label: string
  nodeIds: string[]
}

export interface SpaceModel {
  /** 有名字的空间，按各自第一个节点在配对顺序里的位置排。 */
  spaces: Space[]
  /** 没有分组的节点（不构成命名的空间）。 */
  ungrouped: string[]
  /** 有任何一个节点带分组：没有的话界面上完全没有「空间」，就是节点过滤器。 */
  hasGroups: boolean
}

interface NodeLike {
  info?: Pick<NodeInfo, 'group'> | undefined
}

/** 从已配对的节点（按配对顺序）和它们的 `node.info`（缓存的也算）推出空间。 */
export function deriveSpaces(order: readonly string[], byId: Readonly<Record<string, NodeLike | undefined>>): SpaceModel {
  const byKey = new Map<string, string[]>()
  const ungrouped: string[] = []
  for (const id of order) {
    const key = groupOf(byId[id]?.info)
    if (key === '') {
      ungrouped.push(id)
      continue
    }
    const list = byKey.get(key)
    if (list) list.push(id)
    else byKey.set(key, [id])
  }
  const seen = new Map<string, number>()
  const spaces: Space[] = [...byKey.entries()].map(([key, nodeIds]) => {
    const clean = cleanHintText(key, GROUP_MAX) || '…'
    const n = (seen.get(clean) ?? 0) + 1
    seen.set(clean, n)
    return { key, label: n === 1 ? clean : `${clean} (${n})`, nodeIds }
  })
  return { spaces, ungrouped, hasGroups: spaces.length > 0 }
}

/** 这个节点所在空间的显示名；没有分组返回空串。 */
export function spaceLabelOf(model: SpaceModel, nodeId: string): string {
  return model.spaces.find((s) => s.nodeIds.includes(nodeId))?.label ?? ''
}

/**
 * 当前选中哪些节点。选择有三种：
 *   - 一个**空间**（`space` 不是 null）：它的节点集合跟着分组走（节点改了分组、新配对、被忘记，选中的节点随之变）；
 *     这个空间已经没有节点（分组被清了、改名了）时退回下面两种，偏好里的空间名保留（名字回来就又生效）；
 *   - 手点的一组节点（`saved`）：就是这几个节点，之后配对的新节点不在内；已经没有存在的节点时退回全部；
 *   - 全部（`saved` 也是 null）：之后配对的新节点也在内。
 * 没有任何节点带分组时 `space` 被忽略：和只有节点过滤器的时候一模一样。
 */
export function resolveSelection(order: readonly string[], byId: Readonly<Record<string, NodeLike | undefined>>, saved: readonly string[] | null, space: string | null): string[] {
  if (space !== null) {
    const model = deriveSpaces(order, byId)
    const hit = model.spaces.find((s) => s.key === space)
    if (hit && model.hasGroups) return [...hit.nodeIds]
  }
  const picked = saved ? order.filter((id) => saved.includes(id)) : [...order]
  return picked.length > 0 ? picked : [...order]
}

/** 校验一个分组名（和 daemon 一样）：空 = 没有分组；有的话 1–40 个码点、单行、没有控制字符和 U+2028 / U+2029。 */
// eslint-disable-next-line no-control-regex
const BAD_CHARS = new RegExp('[\\u0000-\\u001f\\u007f-\\u009f\\u2028\\u2029]', 'u')
export function groupError(raw: string): 'too_long' | 'control' | null {
  const v = raw.trim()
  if (v === '') return null
  if (BAD_CHARS.test(v)) return 'control'
  return [...v].length > GROUP_MAX ? 'too_long' : null
}
