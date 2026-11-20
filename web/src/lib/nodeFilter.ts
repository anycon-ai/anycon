import { useMemo } from 'react'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { deriveSpaces, resolveSelection, type SpaceModel } from './spaces'

/** 现在选中哪些节点（不在 React 里的调用方——应用角标、快速记一项——用这个）：口径见 `resolveSelection`。 */
export function currentSelection(): string[] {
  const { order, byId } = useNodes.getState()
  const { sidebarNodes, sidebarSpace } = usePrefs.getState()
  return resolveSelection(order, byId, sidebarNodes, sidebarSpace)
}

/** 现在选中的空间（key）；没选空间、或没有任何节点带分组时是 null。 */
export function currentSpace(): string | null {
  const { order, byId } = useNodes.getState()
  const saved = usePrefs.getState().sidebarSpace
  if (saved === null) return null
  const model = deriveSpaces(order, byId)
  return model.hasGroups && model.spaces.some((s) => s.key === saved) ? saved : null
}

/**
 * 左栏显示哪些节点（空间 / 节点过滤器的选择，命令面板的内容搜索、待处理、清单也只看这些）：
 * 偏好里存的是一个空间的名字（`sidebarSpace`），或手点的节点（`sidebarNodes`，null = 全部）。
 * 选中的节点里已经没有存在的节点（节点被移除）时退回全部，免得左栏一片空白；至少留一个，取消最后一个勾选不生效。
 */
export function useNodeFilter(): {
  selected: string[]
  /** 选中的是哪个空间（key）；手点的一组节点和「全部」是 null。 */
  space: string | null
  spaces: SpaceModel
  toggle: (id: string, on: boolean) => void
  /** 选一个空间（它的节点集合跟着分组走）。 */
  selectSpace: (key: string) => void
  /** 选「全部」。 */
  selectAll: () => void
} {
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const saved = usePrefs((s) => s.sidebarNodes)
  const savedSpace = usePrefs((s) => s.sidebarSpace)
  const setPrefs = usePrefs((s) => s.set)
  const spaces = useMemo(() => deriveSpaces(order, byId), [order, byId])
  const selected = useMemo(() => resolveSelection(order, byId, saved, savedSpace), [order, byId, saved, savedSpace])
  const space = spaces.hasGroups && savedSpace !== null && spaces.spaces.some((s) => s.key === savedSpace) ? savedSpace : null
  const toggle = (id: string, on: boolean): void => {
    const next = order.filter((x) => (x === id ? on : selected.includes(x)))
    if (next.length === 0) return
    // 手点节点：不再是「一个空间」，成了手点的一组
    setPrefs({ sidebarSpace: null, sidebarNodes: next.length === order.length ? null : next })
  }
  const selectSpace = (key: string): void => setPrefs({ sidebarSpace: key, sidebarNodes: null })
  const selectAll = (): void => setPrefs({ sidebarSpace: null, sidebarNodes: null })
  return { selected, space, spaces, toggle, selectSpace, selectAll }
}

/** 只按手点的勾选算（没有空间）：旧的节点过滤器的口径，留给只认节点勾选的调用方和测试。 */
export function selectedNodes(order: readonly string[], saved: readonly string[] | null): string[] {
  return resolveSelection(order, {}, saved, null)
}
