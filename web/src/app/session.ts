import { ApiError, apiFetch } from '@/api/client'
import { logout } from '@/api/queries'
import * as db from '@/db'
import { revokeDeviceOnNode } from '@/features/devices/revoke'
import { dropLocalPushState, dropPushOnSignOut } from '@/features/push/cleanup'
import { forgetSystemNotifyPrefs } from '@/features/push/prefsCache'
import { getClient, rpc } from '@/transport/instance'
import { resetDrafts } from '@/stores/drafts'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { removeThisDevice, type RemoveDeviceReport } from './removeDevice'
import { stopTransport } from './bootstrap'
import { announceWiped } from './tabSync'

/**
 * 普通退出登录（spec/web.md § 4.9）：先退订本浏览器的推送并删 hub 里这台设备的订阅（要登录态，所以在前），再 `POST /auth/logout`。
 * 推送那一步尽力而为，不会让退出失败；`logout` 失败会抛出（调用方提示「你仍处于登录状态」）。
 */
export async function signOut(): Promise<void> {
  const snap = getClient().snapshot
  const deviceId = snap.deviceId ?? (await db.getIdentity().catch(() => undefined))?.device_id ?? null
  await dropPushOnSignOut(deviceId)
  await logout()
}

/** 退出登录之后：停传输层并清掉内存里的节点 / 会话。IndexedDB 保留（设备 token、私钥、会话缓存），同一账号再登录还能用。 */
export async function resetClientState(): Promise<void> {
  stopTransport()
  forgetSystemNotifyPrefs() // 偏好是账号的：换个账号登录不能沿用上一个账号缓存的开关
  useNodes.setState({ byId: {}, order: [], loaded: false })
  useSessions.setState({ list: {}, pending: {}, open: {} })
}

/** 清掉这个浏览器里的一切：停传输层、删 IndexedDB、清 localStorage / sessionStorage。 */
export async function wipeLocalData(): Promise<void> {
  announceWiped() // 先让其它标签页停手，再删库
  await resetClientState()
  resetDrafts()
  await dropLocalPushState() // 浏览器里的推送订阅也退掉（hub 吊销设备时已经删了它那边的）
  await db.resetDb()
  for (const store of [localStorage, sessionStorage]) {
    try {
      store.clear()
    } catch (e) {
      console.warn('anycon: could not clear web storage', e)
    }
  }
}

/** 退出并移除本设备：撤销（节点 + hub）、退出登录、清空本地数据。`force`：撤销 / 退出失败也照样清本地。 */
export function signOutAndRemoveDevice(opts: { force?: boolean } = {}): Promise<RemoveDeviceReport> {
  const client = getClient()
  return removeThisDevice(
    {
      deviceId: client.snapshot.paired ? client.snapshot.deviceId : null,
      nodeIds: useNodes.getState().order,
      isReady: (nodeId) => rpc().isReady(nodeId),
      // not_found（上一次已经删掉了）当作已完成：重试 / force 之后的报告才不会误报「那边还留着公钥」
      revokeOnNode: (nodeId, deviceId) => revokeDeviceOnNode(nodeId, deviceId, 10_000),
      revokeAtHub: async (deviceId) => {
        try {
          await apiFetch(`/api/devices/${encodeURIComponent(deviceId)}`, { method: 'DELETE' })
        } catch (e) {
          if (!(e instanceof ApiError && e.status === 404)) throw e // 404：hub 那边已经没有这台设备了
        }
      },
      logout,
      wipe: wipeLocalData,
    },
    opts,
  )
}
