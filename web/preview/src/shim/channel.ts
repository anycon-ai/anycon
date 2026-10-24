// 页面 ↔ service worker 的消息通道。页面发给 `navigator.serviceWorker.controller`，worker 用 `client.postMessage` 回，
// 回来的消息在 `navigator.serviceWorker` 上以 `message` 事件到达。
import { parseWorkerMessage, type PageMessage, type WorkerMessage } from '../shared/messages'

export interface Channel {
  /** 发给 service worker。当前页面没有被它控制（没有 controller）返回 false。 */
  post(message: PageMessage, transfer?: Transferable[]): boolean
  /** 订阅 service worker 发来的消息；返回取消订阅的函数。 */
  subscribe(listener: (message: WorkerMessage) => void): () => void
}

export function serviceWorkerChannel(container: ServiceWorkerContainer): Channel {
  return {
    post(message, transfer) {
      const controller = container.controller
      if (!controller) return false
      controller.postMessage(message, transfer ?? [])
      return true
    },
    subscribe(listener) {
      const handler = (event: MessageEvent): void => {
        const message = parseWorkerMessage(event.data)
        if (message) listener(message)
      }
      container.addEventListener('message', handler)
      return () => container.removeEventListener('message', handler)
    },
  }
}
