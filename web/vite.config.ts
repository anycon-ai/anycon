import { existsSync, readFileSync } from 'node:fs'
import { fileURLToPath, URL } from 'node:url'
import tailwindcss from '@tailwindcss/vite'
import { tanstackRouter } from '@tanstack/router-plugin/vite'
import react from '@vitejs/plugin-react'
import { VitePWA } from 'vite-plugin-pwa'
import { defaultClientConditions, defineConfig, type Plugin } from 'vite'
import { thirdPartyLicenses } from './scripts/third-party-licenses'

const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')) as { version: string }
const hub = process.env.ANYCON_HUB_URL ?? 'http://127.0.0.1:8080'

/**
 * Noise wasm（`just gen-wasm` 的产物，gitignore）还没构建时，让 typecheck / lint / test / build 仍然能过：
 * 把 `anycon_crypto.js` 换成一个运行时才报错的桩（启动页会显示「wasm 没构建」）。wasm 在就用真的。
 * `ANYCON_WASM_JS` 可以指向别处，测试这条回退用。
 */
function wasmFallback(): Plugin {
  const real = process.env.ANYCON_WASM_JS ?? fileURLToPath(new URL('../packages/protocol/wasm/anycon_crypto.js', import.meta.url))
  const stub = '\0anycon-wasm-missing'
  return {
    name: 'anycon-wasm-fallback',
    enforce: 'pre',
    resolveId(source) {
      if (/(^|\/)anycon_crypto\.js$/.test(source) && !existsSync(real)) {
        this.warn('anycon crypto wasm is not built (packages/protocol/wasm); the web app will fail at startup. Run `just gen-wasm`.')
        return stub
      }
    },
    load(id) {
      if (id === stub) return 'export default async function init() { throw new Error("anycon crypto wasm is not built: run `just gen-wasm`") }'
    },
  }
}

// 开发时把 hub 的 HTTP / WS 表面代理到本地 hub（默认 :8080），浏览器只看到同一个 origin。
export default defineConfig({
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
    // workspace 包 @anycon/protocol 直接用 TS 源码（不依赖它的 dist 构建产物）
    conditions: ['source', ...defaultClientConditions],
  },
  plugins: [
    wasmFallback(),
    // dist/THIRD-PARTY-LICENSES.txt。service worker 单独构建，CSS 里的 preflight 来自 Tailwind，这几个包要显式列出。
    thirdPartyLicenses({ extra: ['tailwindcss', 'workbox-precaching', 'workbox-routing'] }),
    tanstackRouter({ target: 'react', autoCodeSplitting: true, routesDirectory: './src/routes', generatedRouteTree: './src/routeTree.gen.ts' }),
    react(),
    tailwindcss(),
    VitePWA({
      // service worker 是自己写的 `src/sw.ts`（预缓存 + 导航回退 + push / notificationclick / pushsubscriptionchange），
      // 构建时只往里注入预缓存清单（spec/web.md § 8，ADR-37）。
      strategies: 'injectManifest',
      srcDir: 'src',
      filename: 'sw.ts',
      // 新 service worker 就绪后不自动刷新，由 UI 横幅让用户决定（spec/web.md § 8）；`sw.ts` 只响应页面发来的 SKIP_WAITING。
      registerType: 'prompt',
      injectRegister: false,
      manifest: {
        name: 'Anycon',
        short_name: 'Anycon',
        description: 'Remote control for your coding agents',
        display: 'standalone',
        start_url: '/',
        scope: '/',
        background_color: '#0b0d10',
        theme_color: '#0b0d10',
        icons: [
          { src: '/icon-192.png', sizes: '192x192', type: 'image/png' },
          { src: '/icon-512.png', sizes: '512x512', type: 'image/png' },
          { src: '/icon-maskable-512.png', sizes: '512x512', type: 'image/png', purpose: 'maskable' },
        ],
      },
      injectManifest: {
        globPatterns: ['**/*.{js,css,html,svg,png,wasm,webmanifest}'],
        maximumFileSizeToCacheInBytes: 6 * 1024 * 1024,
      },
    }),
  ],
  server: {
    port: 5173,
    proxy: {
      '/api': { target: hub, changeOrigin: false },
      '/auth': { target: hub, changeOrigin: false },
      '/healthz': { target: hub, changeOrigin: false },
      '/readyz': { target: hub, changeOrigin: false },
      '/v1': { target: hub, ws: true, changeOrigin: false },
    },
  },
  build: {
    target: 'es2022',
    sourcemap: false,
    chunkSizeWarningLimit: 900,
  },
})
