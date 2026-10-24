import { useRef } from 'react'
import { useNewProject } from '@/stores/newProject'
import { NewProjectDialog } from './NewProjectDialog'

/** 左栏、命令面板、新建会话页共用的「新建项目」对话框（`useNewProject().open(...)` 打开）。挂在 AppShell 里，任何页面都能开。 */
export function NewProjectHost() {
  const request = useNewProject((s) => s.request)
  const close = useNewProject((s) => s.close)
  const setNode = useNewProject((s) => s.setNode)
  // 关掉之后还要渲染一会儿（关闭动画），用最后一次的节点
  const last = useRef<string | null>(null)
  if (request) last.current = request.nodeId
  const nodeId = request?.nodeId ?? last.current
  if (!nodeId) return null
  return (
    <NewProjectDialog
      nodeId={nodeId}
      open={request !== null}
      onOpenChange={(o) => {
        if (!o) close()
      }}
      initialMode={request?.mode}
      then={request?.then}
      onNodeChange={request?.pickNode ? setNode : undefined}
    />
  )
}
