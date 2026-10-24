// 页面自己的 service worker（spec/web.md § 13.2 shim「service worker」）：预览用 scope `/` 的 service worker 在转发
// 全部请求，dev app 再注册一个会顶掉它。所以 `register` 直接拒绝；dev app 里常见的「注销所有 service worker」
// 清理代码（`getRegistrations().then(rs => rs.forEach(r => r.unregister()))`）也不该碰到预览的那个，所以这两个查询返回空。
export function blockServiceWorkerRegistration(container: ServiceWorkerContainer | undefined, warn: (message: string) => void): () => void {
  if (!container) return () => {}
  const own = ['register', 'getRegistration', 'getRegistrations'] as const
  const saved = own.map((name) => [name, Object.getOwnPropertyDescriptor(container, name)] as const)
  const message = 'Anycon preview: this page cannot register its own service worker, because the preview already runs one for this origin.'
  Object.assign(container, {
    register: () => {
      warn(message)
      return Promise.reject(new DOMException(message, 'SecurityError'))
    },
    getRegistration: () => Promise.resolve(undefined),
    getRegistrations: () => Promise.resolve([]),
  })
  return () => {
    for (const [name, descriptor] of saved) {
      if (descriptor) Object.defineProperty(container, name, descriptor)
      else delete (container as unknown as Record<string, unknown>)[name]
    }
  }
}
