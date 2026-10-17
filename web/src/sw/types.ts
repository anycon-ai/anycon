// service worker 逻辑用到的浏览器接口，写成结构化的最小类型：这样 `src/sw/*.ts` 里的纯逻辑可以在 jsdom 里用假对象测试，
// 也不必把 DOM 和 WebWorker 两套 lib 类型混进同一个 tsconfig。真实的 `ServiceWorkerGlobalScope` 满足这些形状（`src/sw.ts` 里做一次转换）。

export interface ExtendableEventLike {
  waitUntil(promise: Promise<unknown>): void
}

export interface PushEventLike extends ExtendableEventLike {
  readonly data: { json(): unknown } | null
}

export interface NotificationLike {
  readonly data: unknown
  readonly tag: string
  close(): void
}

export interface NotificationEventLike extends ExtendableEventLike {
  readonly notification: NotificationLike
  readonly action: string
}

export interface MessageEventLike extends ExtendableEventLike {
  readonly data: unknown
}

export interface PushSubscriptionLike {
  readonly endpoint: string
  readonly options?: { readonly applicationServerKey: ArrayBuffer | null }
  toJSON(): { endpoint?: string; keys?: Record<string, string> }
}

export interface SubscriptionChangeEventLike extends ExtendableEventLike {
  readonly oldSubscription: PushSubscriptionLike | null
  readonly newSubscription: PushSubscriptionLike | null
}

export interface ShowNotificationOptions {
  body?: string
  tag: string
  icon?: string
  /** 同一个 tag 的通知被新的顶掉时再提醒一次。 */
  renotify?: boolean
  lang?: string
  data: unknown
}

export interface WindowClientLike {
  readonly url: string
  readonly focused?: boolean
  readonly visibilityState?: string
  focus(): Promise<unknown>
  postMessage(message: unknown): void
}

export interface SwEventMap {
  push: PushEventLike
  notificationclick: NotificationEventLike
  pushsubscriptionchange: SubscriptionChangeEventLike
  message: MessageEventLike
}

export interface SwScope {
  readonly location: { readonly origin: string }
  readonly registration: {
    showNotification(title: string, options: ShowNotificationOptions): Promise<void>
    readonly pushManager: {
      subscribe(options: { userVisibleOnly: boolean; applicationServerKey: BufferSource | string }): Promise<PushSubscriptionLike>
    }
  }
  readonly clients: {
    matchAll(options: { type: 'window'; includeUncontrolled: boolean }): Promise<readonly WindowClientLike[]>
    openWindow(url: string): Promise<unknown>
  }
  skipWaiting(): Promise<void>
  addEventListener<K extends keyof SwEventMap>(type: K, listener: (event: SwEventMap[K]) => void): void
}
