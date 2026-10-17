// 「退出并移除本设备」（docs/spec/web.md § 4.9「退出并移除本设备」）：普通退出登录只清内存，IndexedDB 里的设备 token、Noise 私钥和明文会话缓存还在，
// 在共用电脑上别人打开 devtools 就能拿去驱动你的节点。这里把设备真正撤销并清干净。
//
// 顺序：
//   1. 对每个已配对节点调 `node.revoke_device`（让 daemon 删掉本设备的公钥）。**任何节点失败（含离线）都先停下**，
//      告诉用户是哪些节点，且什么都不删——撤销了 hub 这边之后就没法重试了；用户可以选「仍然清除本地数据」（force）；
//   2. `DELETE /api/devices/:id`（hub 删路由 / 配对）；
//   3. `POST /auth/logout`；
//   4. 清空 IndexedDB + localStorage + sessionStorage。
// 2、3 失败同样停下（force 才继续）。先退出登录再清库：退出失败时本地数据还在，可以重试。

export type RemoveStep = 'nodes' | 'hub' | 'logout'

export class RemoveDeviceError extends Error {
  constructor(
    readonly step: RemoveStep,
    override readonly cause: unknown,
    /** step = 'nodes'：没能撤销的节点 id。 */
    readonly failedNodes: string[] = [],
  ) {
    super(`remove device failed at ${step}: ${cause instanceof Error ? cause.message : String(cause)}`)
    this.name = 'RemoveDeviceError'
  }
}

export interface RemoveDeviceDeps {
  /** 本设备的正式 id；从没配对过是 null。 */
  deviceId: string | null
  nodeIds: string[]
  isReady(nodeId: string): boolean
  revokeOnNode(nodeId: string, deviceId: string): Promise<void>
  revokeAtHub(deviceId: string): Promise<void>
  logout(): Promise<void>
  wipe(): Promise<void>
}

export interface RemoveDeviceReport {
  /** daemon 已经删掉本设备公钥的节点。 */
  revokedOnNodes: string[]
  /** force 时放过的节点：它们那边还留着本设备的公钥，只是 hub 不再路由。 */
  skippedNodes: string[]
}

export async function removeThisDevice(d: RemoveDeviceDeps, opts: { force?: boolean } = {}): Promise<RemoveDeviceReport> {
  const report: RemoveDeviceReport = { revokedOnNodes: [], skippedNodes: [] }
  const id = d.deviceId
  if (id) {
    const errors: unknown[] = []
    for (const nodeId of d.nodeIds) {
      if (!d.isReady(nodeId)) {
        report.skippedNodes.push(nodeId)
        errors.push(new Error('node is offline'))
        continue
      }
      try {
        await d.revokeOnNode(nodeId, id)
        report.revokedOnNodes.push(nodeId)
      } catch (e) {
        console.warn(`anycon: node ${nodeId.slice(0, 8)} did not revoke this device`, e)
        report.skippedNodes.push(nodeId)
        errors.push(e)
      }
    }
    if (report.skippedNodes.length > 0 && !opts.force) throw new RemoveDeviceError('nodes', errors[0], report.skippedNodes)
    try {
      await d.revokeAtHub(id)
    } catch (e) {
      if (!opts.force) throw new RemoveDeviceError('hub', e)
      console.warn('anycon: the hub did not revoke this device; wiping local data anyway', e)
    }
  }
  try {
    await d.logout()
  } catch (e) {
    if (!opts.force) throw new RemoveDeviceError('logout', e)
    console.warn('anycon: sign-out request failed; wiping local data anyway', e)
  }
  await d.wipe()
  return report
}
