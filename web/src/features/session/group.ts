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

export function isActiveStatus(s: SessionItem['status']): boolean {
  return s === 'running' || s === 'starting' || s === 'waiting_permission' || s === 'queued'
}
