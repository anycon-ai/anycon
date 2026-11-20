import { useNodeFilter } from '@/lib/nodeFilter'
import { openCount } from './list'
import { useSupportingNodes, useTodoView } from './store'

/** 导航项上的数字：勾选的、支持清单的节点上没关闭的项（不含还没人收下的，它们在「待处理」的稍后段）。 */
export function useOpenTodoCount(): number {
  const { selected } = useNodeFilter()
  const supporting = useSupportingNodes(selected)
  return openCount(useTodoView(supporting))
}
