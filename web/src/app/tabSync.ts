// 多标签页之间的 wipe 同步。
//
// 「退出并移除本设备」会删掉整个 IndexedDB（私钥、device token、会话缓存）。同一个浏览器里的其它标签页如果还开着，
// 它们的传输层 / 草稿 / 同步逻辑还会继续写库——`openDB` 会把刚删掉的库（连同私钥）又建出来。所以发起 wipe 的标签页广播一声，
// 其它标签页收到后停止写库、停传输层、整页跳到登录页。
// 用 BroadcastChannel；没有它的环境退到 `storage` 事件（写一个键再立刻删掉，不留痕迹）。
// 另外 IndexedDB 自己的 `versionchange`（别的标签页在删库）也会触发同一个回调，见 `db.setBlockedHandler`。

const CHANNEL = 'anycon.tabs'
const SIGNAL_KEY = 'anycon.wipe_signal'

type Message = { type: 'wiped' }

let channel: BroadcastChannel | null | undefined

function getChannel(): BroadcastChannel | null {
  if (channel !== undefined) return channel
  try {
    channel = typeof BroadcastChannel === 'function' ? new BroadcastChannel(CHANNEL) : null
  } catch {
    channel = null
  }
  return channel
}

/** 本标签页即将 / 已经清空本地数据：通知其它标签页。 */
export function announceWiped(): void {
  const msg: Message = { type: 'wiped' }
  try {
    getChannel()?.postMessage(msg)
  } catch (e) {
    console.warn('anycon: could not tell the other tabs about the wipe', e)
  }
  // 兜底：`storage` 事件只发给「别的」标签页。写完立刻删，localStorage 里不留东西。
  try {
    localStorage.setItem(SIGNAL_KEY, String(Date.now()))
    localStorage.removeItem(SIGNAL_KEY)
  } catch {
    /* localStorage 不可用就算了 */
  }
}

/** 其它标签页清空了本地数据时调用 `onWiped`。返回取消监听的函数。 */
export function watchForWipe(onWiped: () => void): () => void {
  const ch = getChannel()
  const onMessage = (ev: MessageEvent<unknown>): void => {
    const d = ev.data as Partial<Message> | null
    if (d && typeof d === 'object' && d.type === 'wiped') onWiped()
  }
  const onStorage = (ev: StorageEvent): void => {
    if (ev.key === SIGNAL_KEY && ev.newValue !== null) onWiped()
  }
  ch?.addEventListener('message', onMessage)
  window.addEventListener('storage', onStorage)
  return () => {
    ch?.removeEventListener('message', onMessage)
    window.removeEventListener('storage', onStorage)
  }
}
