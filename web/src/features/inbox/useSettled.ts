// 左栏勾选的节点这一轮同步是不是都做完了：状态播报取基线、手机「待处理」决定要不要默认展开第一条，都等它。
import { useEffect, useState } from 'react'
import { selectedNodes } from '@/lib/nodeFilter'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'

/** 等一个节点同步完最多等这么久（毫秒），之后不管它了、当作定下来了。 */
export const SETTLE_TIMEOUT_MS = 15_000

/** 节点这一轮同步做完了（`synced`），或者确认它不会有同步：离线、不认识这个浏览器（`noise_failed`）、协议版本不兼容。还在连、正在同步都不算。 */
function isSettled(conn: string, synced: boolean, syncing: boolean, versionMismatch: boolean): boolean {
  if (conn === 'peer_offline' || conn === 'noise_failed' || versionMismatch) return true
  return conn === 'ready' && synced && !syncing
}

export function useNodesSettled(): boolean {
  const byId = useNodes((st) => st.byId)
  const order = useNodes((st) => st.order)
  const saved = usePrefs((st) => st.sidebarNodes)
  const settledNow = selectedNodes(order, saved).every((id) => {
    const n = byId[id]
    return !!n && isSettled(n.conn, n.synced, n.syncing, n.versionMismatch)
  })
  // 有节点一直定不下来（第一次同步失败、卡在连接上）：等一阵子之后也算定下来，不然依赖它的东西永远不动
  const [gaveUp, setGaveUp] = useState(false)
  useEffect(() => {
    if (settledNow) {
      setGaveUp(false)
      return
    }
    const id = setTimeout(() => setGaveUp(true), SETTLE_TIMEOUT_MS)
    return () => clearTimeout(id)
  }, [settledNow])
  return settledNow || gaveUp
}
