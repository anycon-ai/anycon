// 页面在后台满 30 秒就主动断开和 hub 的连接，回到前台再连（spec/web.md § 7）。
//
// 为什么：hub 只给「没有在线连接」的设备推 Web Push。手机上退到后台的页面会被冻结 / 挂起，WebSocket 的 ping / pong
// 却由浏览器的网络层应答，连接一直算在线——`notify` 被转发进一个没人读的连接，推送一条也不发。主动断开之后 hub 才看得出
// 这台浏览器「不在」。30 秒是个折中：切出去看一眼别的应用再回来不会断，收到的 `notify` 在这 30 秒里由页面自己弹系统通知。
//
// 这里只管计时；断开本身和「眼下不能断」的判断（正在配对 / 接入、还有请求在路上）在 `AnyconClient.suspendForBackground()`。

/** 页面 hidden 多久之后断开 hub 连接。 */
export const BACKGROUND_DISCONNECT_MS = 30_000
/** 眼下不能断（配对 / 接入 / 请求在路上）时，多久之后再试。 */
export const BACKGROUND_RETRY_MS = 5_000

export interface BackgroundClient {
  /** 断开并停止自动重连；返回 false = 眼下不能断，稍后再试。 */
  suspendForBackground(): boolean
  /** 页面回到前台：断着的立即重连并重新同步。 */
  onVisible(): Promise<void>
}

export interface BackgroundDeps {
  client(): BackgroundClient
  visibility(): DocumentVisibilityState
  setTimer(fn: () => void, ms: number): unknown
  clearTimer(handle: unknown): void
}

export interface BackgroundWatcher {
  /** `visibilitychange` 的处理：hidden 开始计时，visible 取消计时并让客户端重连。 */
  onVisibilityChange(): void
  /** 启动时调用：页面本来就在后台（比如在后台标签页里打开的）也要计时。 */
  arm(): void
  /** 停掉计时（传输层停了）。 */
  dispose(): void
}

export function createBackgroundWatcher(deps: BackgroundDeps): BackgroundWatcher {
  let timer: unknown = null

  const cancel = (): void => {
    if (timer !== null) deps.clearTimer(timer)
    timer = null
  }
  const schedule = (ms: number): void => {
    cancel()
    timer = deps.setTimer(() => {
      timer = null
      if (deps.visibility() !== 'hidden') return
      if (!deps.client().suspendForBackground()) schedule(BACKGROUND_RETRY_MS)
    }, ms)
  }

  return {
    onVisibilityChange() {
      if (deps.visibility() === 'visible') {
        cancel()
        void deps.client().onVisible()
      } else if (timer === null) {
        schedule(BACKGROUND_DISCONNECT_MS)
      }
    },
    arm() {
      if (deps.visibility() === 'hidden' && timer === null) schedule(BACKGROUND_DISCONNECT_MS)
    },
    dispose: cancel,
  }
}
