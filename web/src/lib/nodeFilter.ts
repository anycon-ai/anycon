import { useMemo } from 'react'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'

/**
 * 左栏显示哪些节点（节点过滤器的勾选，命令面板的内容搜索也只搜这些）：偏好里存的是用户勾选的节点（null = 全部）。
 * 勾选里已经没有存在的节点（节点被移除）时退回全部，免得左栏一片空白；至少留一个，取消最后一个勾选不生效。
 */
export function useNodeFilter(): { selected: string[]; toggle: (id: string, on: boolean) => void } {
  const order = useNodes((s) => s.order)
  const saved = usePrefs((s) => s.sidebarNodes)
  const setPrefs = usePrefs((s) => s.set)
  const selected = useMemo(() => {
    const picked = saved ? order.filter((id) => saved.includes(id)) : order
    return picked.length > 0 ? picked : order
  }, [saved, order])
  const toggle = (id: string, on: boolean): void => {
    const next = order.filter((x) => (x === id ? on : selected.includes(x)))
    if (next.length === 0) return
    setPrefs({ sidebarNodes: next.length === order.length ? null : next })
  }
  return { selected, toggle }
}
