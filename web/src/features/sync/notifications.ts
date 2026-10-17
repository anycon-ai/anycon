// daemon → 客户端的通知分发（session.event / session.changed / permission.* / agent.changed / workspace.changed / job.*）。
import type { AgentListResult, JobDone, JobProgress, NodeDevicesResult, NodeEnrollResolved, PermissionRequest, PermissionResolved, SessionChanged, SessionEvent, SessionSummary, WorkspaceChanged, WorkspaceListResult } from '@/protocol'
import { rpc } from '@/transport/instance'
import { enqueueLive } from '@/features/session/actions'
import { parseEnrollInfo, useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import i18n from '@/i18n'
import { handleSecurityEvent } from './securityEvent'

export function handleNotification(nodeId: string, method: string, params: unknown): void {
  switch (method) {
    case 'session.event': {
      // 事件只会发给 watcher：必须是这个节点打开的会话。别的节点（或没打开的会话）发来的一律丢弃，不能向别的节点的会话里注入事件
      const ev = params as SessionEvent
      const open = useSessions.getState().open[ev.session_id]
      if (!open || open.nodeId !== nodeId) {
        console.warn(`anycon: dropped a session.event from node ${nodeId.slice(0, 8)} for a session it does not own or that is not open`)
        return
      }
      enqueueLive(ev)
      return
    }
    case 'session.changed': {
      const ch = params as SessionChanged
      const known = useSessions.getState().list[ch.session_id]
      if (known && known.node_id !== nodeId) {
        console.warn(`anycon: dropped a session.changed from node ${nodeId.slice(0, 8)} for a session of another node`)
        return
      }
      useSessions.getState().applyChanged(nodeId, ch)
      if (!known && !ch.deleted) {
        // 别的设备新建的会话：补一次元数据
        void rpc()
          .request<SessionSummary>(nodeId, 'session.get', { session_id: ch.session_id })
          .then((s) => useSessions.getState().upsertSummaries(nodeId, [s]))
          .catch((e: unknown) => console.warn('anycon: could not fetch a session announced by session.changed', e))
      }
      return
    }
    case 'permission.request': {
      const req = params as PermissionRequest
      useSessions.getState().addPending(nodeId, req)
      // 页面在前台：不弹系统通知，改为应用内 toast（正在看那个会话时不打扰）
      if (!useSessions.getState().open[req.session_id]) {
        const title = useSessions.getState().list[req.session_id]?.title
        toast.info(i18n.t('toast.permission', { title: title ? `“${title}”` : i18n.t('toast.aSession') }))
      }
      return
    }
    case 'permission.resolved': {
      const res = params as PermissionResolved
      useSessions.getState().resolvePending(nodeId, res.request_id)
      // 答复的设备是这个标签页还不认识的（比如配对之后才加入的第二台）：补拉一次设备列表，卡片才能显示设备名而不是短 id。
      const by = res.by_device
      const known = useNodes.getState().byId[nodeId]?.devices ?? []
      if (by && by !== 'daemon' && by !== useHub.getState().snapshot.deviceId && !known.some((d) => d.device_id === by)) {
        void refreshDevices(nodeId)
      }
      return
    }
    case 'node.enroll_requested': {
      // 有设备请求接入（ADR-36）：存进 store，StatusBanner 的横幅（§ 3.4）据此出现，点「查看」打开批准对话框
      const info = parseEnrollInfo(params)
      if (info) useEnroll.getState().upsert(nodeId, info)
      else console.warn('anycon: ignored a malformed node.enroll_requested')
      return
    }
    case 'node.enroll_resolved': {
      // 别的设备先处理了 / 已完成 / 已过期：让批准卡片收起
      const r = params as Partial<NodeEnrollResolved> | null
      if (r && typeof r.enroll_id === 'string' && typeof r.result === 'string') useEnroll.getState().resolve(nodeId, r.enroll_id, r.result, r.by_device ?? null)
      else console.warn('anycon: ignored a malformed node.enroll_resolved')
      return
    }
    case 'node.security_event':
      handleSecurityEvent(nodeId, params)
      return
    case 'agent.changed': {
      const res = params as Partial<AgentListResult> | null
      if (res?.agents && res.etag) useNodes.getState().setAgents(nodeId, res.agents, res.etag)
      else
        void rpc()
          .request<AgentListResult>(nodeId, 'agent.list', {})
          .then((r) => useNodes.getState().setAgents(nodeId, r.agents, r.etag))
          .catch((e: unknown) => console.warn('anycon: could not refresh the agent list after agent.changed', e))
      return
    }
    case 'workspace.changed': {
      useNodes.getState().applyWorkspaceChanged(nodeId, params as WorkspaceChanged)
      void refreshProjects(nodeId)
      return
    }
    case 'job.progress': {
      const p = params as JobProgress
      useNodes.getState().jobProgress(nodeId, { id: p.job_id, stage: p.stage, percent: p.percent, log: p.log_line })
      return
    }
    case 'job.done': {
      const d = params as JobDone
      useNodes.getState().jobProgress(nodeId, { id: d.job_id, stage: 'done', done: true, ok: d.ok, error: d.error, percent: d.ok ? 100 : undefined })
      if (d.ok) void refreshProjects(nodeId)
      return
    }
  }
}

export async function refreshDevices(nodeId: string): Promise<void> {
  try {
    const res = await rpc().request<NodeDevicesResult>(nodeId, 'node.devices', {})
    useNodes.getState().setDevices(nodeId, res.devices)
  } catch (e) {
    console.warn('anycon: could not refresh the device list; keeping the cached one', e)
  }
}

// 项目列表的请求可能交叉：补拉 git 元数据和 `workspace.changed` 各发一次，旧的那份可能晚到。
// 只采用「已采用的最新一份」之后发出的结果，晚到的旧结果（可能还没有 `git`）不能盖掉新的
const listIssued = new Map<string, number>()
const listApplied = new Map<string, number>()

export async function refreshProjects(nodeId: string, opts: { timeoutMs?: number } = {}): Promise<void> {
  const seq = (listIssued.get(nodeId) ?? 0) + 1
  listIssued.set(nodeId, seq)
  try {
    const ws = await rpc().request<WorkspaceListResult>(nodeId, 'workspace.list', {}, opts.timeoutMs)
    if (seq < (listApplied.get(nodeId) ?? 0)) return
    listApplied.set(nodeId, seq)
    useNodes.getState().setProjects(nodeId, ws.root, ws.projects)
  } catch (e) {
    console.warn('anycon: could not refresh the project list; keeping the cached one', e)
  }
}
