// 登录后启动传输层，并把它的回调接到各个 store / 同步逻辑上。
import * as db from '@/db'
import { handleNotification } from '@/features/sync/notifications'
import { hydrateNodeSessions, syncNode } from '@/features/sync/syncNode'
import { showSystemNotification } from '@/features/sync/systemNotify'
import { getClient } from '@/transport/instance'
import { resetDrafts, useDrafts } from '@/stores/drafts'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import i18n from '@/i18n'
import { createBackgroundWatcher, type BackgroundWatcher } from './backgroundIdle'
import { watchForWipe } from './tabSync'

let booting: Promise<void> | null = null
let currentAccount: string | null = null
let unsubBadge: (() => void) | null = null
/** 页面在后台满 30 秒就断开 hub 连接（`./backgroundIdle.ts`）。 */
const background: BackgroundWatcher = createBackgroundWatcher({
  client: getClient,
  visibility: () => document.visibilityState,
  setTimer: (fn, ms) => setTimeout(fn, ms),
  clearTimer: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
})

/** 待审批总数 → 标签页标题前缀 + PWA 图标角标（支持的平台上）。 */
export function updateBadge(count: number): void {
  document.title = count > 0 ? `(${count}) Anycon` : 'Anycon'
  const nav = navigator as Navigator & { setAppBadge?: (n?: number) => Promise<void>; clearAppBadge?: () => Promise<void> }
  // 角标 API 在很多环境里直接 reject（没装成 PWA 等），不影响功能
  if (count > 0) void nav.setAppBadge?.(count).catch((e: unknown) => console.debug('anycon: setAppBadge unavailable', e))
  else void nav.clearAppBadge?.().catch((e: unknown) => console.debug('anycon: clearAppBadge unavailable', e))
}

let wipedElsewhere = false
let stopWatching: (() => void) | null = null

/**
 * 另一个标签页清空了本地数据（「退出并移除本设备」）：这个标签页不能再写 IndexedDB——否则会把刚删掉的库
 * （连同私钥 / token）又建出来。先禁用库、停传输层和草稿定时器，再整页跳到登录页（丢掉内存里的一切）。
 */
export function handleWipedElsewhere(goToLogin: () => void = () => location.replace('/login')): void {
  if (wipedElsewhere) return
  wipedElsewhere = true
  db.disableDb()
  stopTransport()
  resetDrafts()
  goToLogin()
}

export function startTransport(accountId: string): Promise<void> {
  if (booting && currentAccount === accountId) return booting
  currentAccount = accountId
  if (!stopWatching) {
    stopWatching = watchForWipe(() => handleWipedElsewhere())
    db.setBlockedHandler(() => handleWipedElsewhere())
  }
  const client = getClient()
  client.setSink({
    hub: (s) => useHub.getState().setSnapshot(s),
    node: (s) => useNodes.getState().setConn(s.nodeId, s.conn),
    nodesLoaded: (nodes) => {
      useNodes.getState().hydrate(nodes)
      for (const n of nodes) void hydrateNodeSessions(n.node_id)
    },
    nodeAdded: (rec) => useNodes.getState().add(rec),
    nodeReady: (id) => void syncNode(id),
    notification: (id, method, params) => handleNotification(id, method, params),
    // 页面在后台时，明文的 notify 信封变成系统通知（页面在前台由 toast / 横幅负责；hub 只给没有在线连接的设备推 Web Push）
    notify: (id, body) => void showSystemNotification(id, body).catch((e: unknown) => console.warn('anycon: could not show a system notification', e)),
    quota: (q) => useHub.getState().setQuota(q ?? null),
    // hub 丢了我们发的东西：「发错了」的信号，每秒最多一条。too_large = 我们没分片（bug）；not_paired = 配对关系不对
    dropped: (reason, nodeId) => {
      if (reason === 'too_large') toast.error(i18n.t('errors.dropped.too_large'))
      else if (reason === 'not_paired') toast.error(i18n.t('errors.dropped.not_paired', { node: (nodeId && useNodes.getState().byId[nodeId]?.name) || i18n.t('errors.dropped.thisNode') }))
    },
  })
  booting = (async () => {
    await client.start(accountId)
    void useDrafts.getState().loadAll() // 横幅里的「N 条未发送」要算上没打开过的会话
    useHub.getState().setStarted(true)
    document.addEventListener('visibilitychange', onVisibility)
    background.arm() // 在后台标签页里打开的页面也算
    unsubBadge?.()
    unsubBadge = useSessions.subscribe((s, prev) => {
      if (s.pending !== prev.pending) updateBadge(Object.keys(s.pending).length)
    })
  })().catch((e: unknown) => {
    booting = null
    throw e
  })
  return booting
}

function onVisibility(): void {
  background.onVisibilityChange()
}

export function stopTransport(): void {
  document.removeEventListener('visibilitychange', onVisibility)
  background.dispose()
  unsubBadge?.()
  unsubBadge = null
  updateBadge(0)
  getClient().stop()
  booting = null
  currentAccount = null
  useHub.getState().setStarted(false)
}
