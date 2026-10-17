import type { JobStarted, Project, WorkspaceBrowseResult, WorkspaceTrashListResult } from '@/protocol'
import { refreshProjects } from '@/features/sync/notifications'
import { rpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'

export async function createProject(nodeId: string, name: string, gitInit: boolean): Promise<void> {
  await rpc().request(nodeId, 'workspace.create', { name, git_init: gitInit })
  await refreshProjects(nodeId)
}

/** 克隆是长任务：返回 job id，进度走 job.progress / job.done 通知（见 notifications.ts）。 */
export async function cloneProject(nodeId: string, url: string, name?: string): Promise<string> {
  const res = await rpc().request<JobStarted>(nodeId, 'workspace.clone', { url, name: name || undefined })
  useNodes.getState().jobProgress(nodeId, { id: res.job_id, stage: 'starting', percent: 0 })
  return res.job_id
}

export async function cancelJob(nodeId: string, jobId: string): Promise<void> {
  await rpc().request(nodeId, 'job.cancel', { job_id: jobId })
}

export async function renameProject(nodeId: string, from: string, to: string): Promise<void> {
  await rpc().request(nodeId, 'workspace.rename', { from, to })
  await refreshProjects(nodeId)
}

export async function trashProject(nodeId: string, name: string): Promise<void> {
  await rpc().request(nodeId, 'workspace.trash', { name })
  await refreshProjects(nodeId)
}

/** 关联本机已有目录为项目（ADR-33）。成功后返回 daemon 给的 `Project`（`name` 是最终项目名）。 */
export async function linkProject(nodeId: string, path: string, name?: string): Promise<Project> {
  const project = await rpc().request<Project>(nodeId, 'workspace.link', { path, name: name || undefined })
  await refreshProjects(nodeId)
  return project
}

/** 从列表移除关联项目：只删登记，不动目录里的文件。 */
export async function unlinkProject(nodeId: string, name: string): Promise<void> {
  await rpc().request(nodeId, 'workspace.unlink', { name })
  await refreshProjects(nodeId)
}

/** 给目录选择器列 `link_roots` 之内的子目录。`path` 缺省 = 第一个 root。 */
export async function browseFolders(nodeId: string, path?: string, showHidden = false): Promise<WorkspaceBrowseResult> {
  return rpc().request<WorkspaceBrowseResult>(nodeId, 'workspace.browse', { path: path || undefined, show_hidden: showHidden })
}

export async function loadTrash(nodeId: string): Promise<void> {
  const res = await rpc().request<WorkspaceTrashListResult>(nodeId, 'workspace.trash_list', {})
  useNodes.getState().setTrash(nodeId, res.entries)
}

export async function restoreTrash(nodeId: string, id: string): Promise<void> {
  await rpc().request(nodeId, 'workspace.restore', { id })
  await Promise.all([loadTrash(nodeId), refreshProjects(nodeId)])
}

export async function purgeTrash(nodeId: string, id: string): Promise<void> {
  await rpc().request(nodeId, 'workspace.purge', { id })
  await loadTrash(nodeId)
}
