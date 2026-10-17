// `session.diff`（只读）：文件列表，以及点开一个文件时它的 unified diff。
import type { DiffAgainst, DiffFile, SessionDiffResult } from '@/protocol'
import { rpc } from '@/transport/instance'

/** 文件列表（≤ 1000 个，只有路径、状态和行数）。 */
export async function loadDiffList(nodeId: string, sessionId: string, against: DiffAgainst): Promise<SessionDiffResult> {
  return rpc().request<SessionDiffResult>(nodeId, 'session.diff', { session_id: sessionId, against })
}

/** 一个文件的 patch（≤ 256 KiB，超出 `patch_truncated`）。 */
export async function loadDiffPatch(nodeId: string, sessionId: string, against: DiffAgainst, path: string): Promise<DiffFile | null> {
  const res = await rpc().request<SessionDiffResult>(nodeId, 'session.diff', { session_id: sessionId, against, path })
  return res.files.find((f) => f.path === path) ?? res.files[0] ?? null
}
