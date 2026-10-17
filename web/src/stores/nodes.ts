import { create } from 'zustand'
import type { AgentInfo, NodeInfo, PeerDevice, Project, TrashEntry, WorkspaceChanged } from '@/protocol'
import { putNode, type NodeAgentDefaults, type NodeRecord } from '@/db'
import { storageFailed } from '@/lib/log'
import type { NodeConnState } from '@/transport/connection'

export interface JobState {
  id: string
  stage: string
  percent?: number | null
  log?: string | null
  done?: boolean
  ok?: boolean
  error?: string | null
}

export interface NodeEntry {
  id: string
  name: string
  publicKey?: Uint8Array
  conn: NodeConnState
  info?: NodeInfo
  agents: AgentInfo[]
  agentsEtag?: string
  agentsLoaded: boolean
  agentDefaults: Record<string, NodeAgentDefaults>
  projects: Project[]
  workspaceRoot?: string
  projectsLoaded: boolean
  trash: TrashEntry[]
  devices: PeerDevice[]
  lastSeen: number
  pairedAt: number
  /** 本次连接是否已完成同步（spec § 6.3）。 */
  synced: boolean
  syncing: boolean
  /** daemon 的协议版本和我们不兼容。 */
  versionMismatch: boolean
  /** daemon 拒绝了危险模式：禁用这些项并说明（没有别的办法知道节点配置）。 */
  dangerousForbidden: boolean
  jobs: Record<string, JobState>
}

interface NodesState {
  byId: Record<string, NodeEntry>
  order: string[]
  loaded: boolean
  hydrate: (records: NodeRecord[]) => void
  add: (rec: NodeRecord) => void
  remove: (id: string) => void
  setConn: (id: string, conn: NodeConnState) => void
  patch: (id: string, patch: Partial<NodeEntry>) => void
  setInfo: (id: string, info: NodeInfo) => void
  setAgents: (id: string, agents: AgentInfo[], etag: string) => void
  setAgentDefaults: (id: string, agent: string, d: NodeAgentDefaults) => void
  setProjects: (id: string, root: string, projects: Project[]) => void
  setTrash: (id: string, entries: TrashEntry[]) => void
  setDevices: (id: string, devices: PeerDevice[]) => void
  applyWorkspaceChanged: (id: string, ch: WorkspaceChanged) => void
  jobProgress: (id: string, job: JobState) => void
}

function entryFromRecord(r: NodeRecord): NodeEntry {
  return {
    id: r.node_id,
    name: r.name,
    publicKey: r.public_key,
    conn: 'hub_disconnected',
    info: r.info,
    agents: r.agents ?? [],
    agentsEtag: r.agents_etag,
    agentsLoaded: !!r.agents,
    agentDefaults: r.agent_defaults ?? {},
    projects: r.projects ?? [],
    workspaceRoot: r.workspace_root,
    projectsLoaded: !!r.projects,
    trash: [],
    devices: [],
    lastSeen: r.last_seen,
    pairedAt: r.paired_at,
    synced: false,
    syncing: false,
    versionMismatch: false,
    dangerousForbidden: false,
    jobs: {},
  }
}

function toRecord(e: NodeEntry): NodeRecord | null {
  if (!e.publicKey) return null
  return {
    node_id: e.id,
    name: e.name,
    public_key: e.publicKey,
    info: e.info,
    agents: e.agents,
    agents_etag: e.agentsEtag,
    agent_defaults: e.agentDefaults,
    projects: e.projectsLoaded ? e.projects : undefined,
    workspace_root: e.workspaceRoot,
    paired_at: e.pairedAt,
    last_seen: e.lastSeen,
  }
}

function persist(e: NodeEntry | undefined): void {
  const rec = e ? toRecord(e) : null
  if (rec) putNode(rec).catch(storageFailed('cache the node state'))
}

export const useNodes = create<NodesState>()((set, get) => {
  const update = (id: string, f: (e: NodeEntry) => NodeEntry, save = false): void => {
    const cur = get().byId[id]
    if (!cur) return
    const next = f(cur)
    set((s) => ({ byId: { ...s.byId, [id]: next } }))
    if (save) persist(next)
  }
  return {
    byId: {},
    order: [],
    loaded: false,
    hydrate: (records) => {
      const sorted = [...records].sort((a, b) => a.paired_at - b.paired_at)
      set({ byId: Object.fromEntries(sorted.map((r) => [r.node_id, entryFromRecord(r)])), order: sorted.map((r) => r.node_id), loaded: true })
    },
    add: (rec) =>
      set((s) => ({
        byId: { ...s.byId, [rec.node_id]: { ...(s.byId[rec.node_id] ?? entryFromRecord(rec)), name: rec.name, publicKey: rec.public_key, pairedAt: rec.paired_at } },
        order: s.order.includes(rec.node_id) ? s.order : [...s.order, rec.node_id],
        loaded: true,
      })),
    remove: (id) =>
      set((s) => {
        const { [id]: _gone, ...rest } = s.byId
        return { byId: rest, order: s.order.filter((x) => x !== id) }
      }),
    setConn: (id, conn) => update(id, (e) => (e.conn === conn ? e : { ...e, conn, ...(conn === 'ready' ? { lastSeen: Date.now() } : {}), ...(conn === 'ready' ? {} : { synced: false, syncing: false }) })),
    patch: (id, patch) => update(id, (e) => ({ ...e, ...patch })),
    setInfo: (id, info) => update(id, (e) => ({ ...e, info, name: info.name || e.name }), true),
    setAgents: (id, agents, etag) => update(id, (e) => ({ ...e, agents, agentsEtag: etag, agentsLoaded: true }), true),
    setAgentDefaults: (id, agent, d) => update(id, (e) => ({ ...e, agentDefaults: { ...e.agentDefaults, [agent]: d } }), true),
    setProjects: (id, root, projects) => update(id, (e) => ({ ...e, workspaceRoot: root, projects, projectsLoaded: true }), true),
    setTrash: (id, trash) => update(id, (e) => ({ ...e, trash })),
    setDevices: (id, devices) => update(id, (e) => ({ ...e, devices })),
    applyWorkspaceChanged: (id, ch) =>
      update(id, (e) => {
        let projects = e.projects.filter((p) => !(ch.removed ?? []).includes(p.name))
        for (const r of ch.renamed ?? []) projects = projects.map((p) => (p.name === r.from ? { ...p, name: r.to } : p))
        return { ...e, projects }
      }, true),
    jobProgress: (id, job) => update(id, (e) => ({ ...e, jobs: { ...e.jobs, [job.id]: { ...e.jobs[job.id], ...job } } })),
  }
})

export function readyAgents(entry: NodeEntry | undefined): AgentInfo[] {
  return (entry?.agents ?? []).filter((a) => a.status === 'ready' && a.enabled)
}
