// Service worker 入口（`vite-plugin-pwa` 的 `injectManifest`，spec/web.md § 8）。
// 预缓存 / 导航回退 / 清理旧缓存 / 「新版本就绪才刷新」和原来的 generateSW 配置行为一致；在这之上加 `push`、`notificationclick`、
// `pushsubscriptionchange`（逻辑都在 `./sw/`，这里只做接线）。
import { cleanupOutdatedCaches, createHandlerBoundToURL, precacheAndRoute, type PrecacheEntry } from 'workbox-precaching'
import { NavigationRoute, registerRoute } from 'workbox-routing'
import { installHandlers, NAVIGATION_DENYLIST } from './sw/install'
import type { SwScope } from './sw/types'

// 注意：`self.__WB_MANIFEST` 必须原样出现——构建时 workbox 按这个字面量找注入点。
precacheAndRoute((self as unknown as { __WB_MANIFEST: Array<string | PrecacheEntry> }).__WB_MANIFEST)
cleanupOutdatedCaches()
registerRoute(new NavigationRoute(createHandlerBoundToURL('/index.html'), { denylist: [...NAVIGATION_DENYLIST] }))

installHandlers(self as unknown as SwScope)
