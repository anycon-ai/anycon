import type { Project } from '@/protocol'
import type { SessionItem } from '@/stores/sessions'

export interface ProjectGroup {
  name: string
  project?: Project
  sessions: SessionItem[]
  /** 该组最近活动时间（会话更新或项目修改）。 */
  activity: number
  pending: number
}

export interface GroupedSessions {
  projects: ProjectGroup[]
  scratch: SessionItem[]
}

/** 左栏的会话树：按项目分组，最近活跃的项目在前；归档的不显示；scratch 单独成组。 */
export function groupSessions(sessions: readonly SessionItem[], projects: readonly Project[], opts: { includeArchived?: boolean } = {}): GroupedSessions {
  const visible = sessions.filter((s) => opts.includeArchived || !s.archived).sort((a, b) => b.updated_at_ms - a.updated_at_ms)
  const byName = new Map<string, ProjectGroup>()
  for (const p of projects) byName.set(p.name, { name: p.name, project: p, sessions: [], activity: p.modified_at_ms, pending: 0 })
  const scratch: SessionItem[] = []
  for (const s of visible) {
    if (!s.project) {
      scratch.push(s)
      continue
    }
    let g = byName.get(s.project)
    if (!g) {
      g = { name: s.project, sessions: [], activity: 0, pending: 0 }
      byName.set(s.project, g)
    }
    g.sessions.push(s)
    g.activity = Math.max(g.activity, s.updated_at_ms)
    g.pending += s.pending_permissions
  }
  const groups = [...byName.values()].sort((a, b) => b.activity - a.activity || a.name.localeCompare(b.name))
  return { projects: groups, scratch }
}

/** 多个节点合并后的项目组：同名项目在不同节点上是两个组，所以带上所属节点。 */
export interface NodeProjectGroup extends ProjectGroup {
  nodeId: string
}

export interface NodeGroupedSessions {
  projects: NodeProjectGroup[]
  scratch: SessionItem[]
}

/** 左栏按节点过滤后的会话树：每个节点各自分组，再合并成一个列表（项目组按最近活动排，临时会话按更新时间排）。 */
export function groupNodeSessions(nodes: readonly { id: string; projects: readonly Project[] }[], sessions: readonly SessionItem[]): NodeGroupedSessions {
  const projects: NodeProjectGroup[] = []
  const scratch: SessionItem[] = []
  for (const n of nodes) {
    const g = groupSessions(
      sessions.filter((s) => s.node_id === n.id),
      n.projects,
    )
    for (const p of g.projects) projects.push({ ...p, nodeId: n.id })
    scratch.push(...g.scratch)
  }
  projects.sort((a, b) => b.activity - a.activity || a.name.localeCompare(b.name) || a.nodeId.localeCompare(b.nodeId))
  scratch.sort((a, b) => b.updated_at_ms - a.updated_at_ms)
  return { projects, scratch }
}

export function isActiveStatus(s: SessionItem['status']): boolean {
  return s === 'running' || s === 'starting' || s === 'waiting_permission' || s === 'queued'
}
