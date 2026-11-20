// 空间（spec/web.md § 4.13、docs/spec/daemon.md § 21.7）：分组名相同的已配对节点是一个空间。分组名是任何已配对设备都能写到节点上的文字，
// 一律当作不可信的显示文本：身份用去掉首尾空白之后的**原文**（区分大小写），显示用清理过的文字；两个原文不同的名字清理之后相同，
// 显示时加序号区分，身份不会撞。纯函数，不依赖 React。
import type { NodeInfo } from '@/protocol'
import { cleanHintText } from './hint'

/** 分组名最多这么多个码点（和 daemon 的 `hub.group` 一致）。 */
export const GROUP_MAX = 40

// 不可见的字符：Unicode 的 Default_Ignorable_Code_Point（零宽空格 / 连接符、词连接符、BOM、变体选择符……）加阿拉伯字母标记 U+061C
// （双向控制符之一）。它们让两个不同的名字看起来一模一样，显示时去掉。
const INVISIBLE = /[\p{Default_Ignorable_Code_Point}\u061C]/gu

/** 分组名的显示文字：NFC 规范化、去掉不可见字符、清理控制字符和双向控制符、最多 40 个码点。身份仍是去掉首尾空白的原文，不是这个。 */
export function displayName(raw: string): string {
  return cleanHintText(raw.normalize('NFC').replace(INVISIBLE, ''), GROUP_MAX)
}

/**
 * 每个空间（身份 = 原文）的显示名。清理之后显示相同的名字（`Home` 和 `Ho[ZWSP]me`、NFC 和 NFD 的 `Café`）按原文排序、第一个用原样（原文就等于显示文字的优先，再按原文排），
 * 其余加序号（`Home (2)`）：序号只由名字本身决定，和节点的配对顺序无关；序号不会撞上别的显示名（有个空间真叫 `Home (2)` 时跳过它）。
 */
function assignLabels(keys: readonly string[]): Map<string, string> {
  const byClean = new Map<string, string[]>()
  for (const key of keys) {
    const clean = displayName(key) || '…'
    const list = byClean.get(clean)
    if (list) list.push(key)
    else byClean.set(clean, [key])
  }
  const out = new Map<string, string>()
  const used = new Set<string>()
  const groups = [...byClean.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
  // 第一轮：每个显示名的第一个用原样，先把所有原样的占住
  for (const [clean, list] of groups) {
    // 原文本来就等于显示文字的名字用原样；清理之后才一样的（不可见字符、NFD……）让位加序号；同一类里按原文排，顺序是稳定的
    list.sort((a, b) => Number(a !== clean) - Number(b !== clean) || (a < b ? -1 : a > b ? 1 : 0))
    out.set(list[0]!, clean)
    used.add(clean)
  }
  // 第二轮：重名的加序号，跳过已经被占的
  for (const [clean, list] of groups) {
    let n = 2
    for (const key of list.slice(1)) {
      while (used.has(`${clean} (${n})`)) n++
      const label = `${clean} (${n})`
      out.set(key, label)
      used.add(label)
      n++
    }
  }
  return out
}

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
  const labels = assignLabels([...byKey.keys()])
  const spaces: Space[] = [...byKey.entries()].map(([key, nodeIds]) => ({ key, label: labels.get(key)!, nodeIds }))
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
