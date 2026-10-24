import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vitest/config'

// 测试用单独的配置：不加载路由生成 / Tailwind / PWA 插件，tsx 交给 vitest 自带的转换。
// 两个 project：Web UI（jsdom + React 测试环境）和预览运行时（`preview/`，node 环境，不加载 Web UI 的 setup；
// 要 DOM 的测试文件用 `// @vitest-environment jsdom` 单独指定）。
export default defineConfig({
  define: { __APP_VERSION__: JSON.stringify('0.0.0-test') },
  resolve: { alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) }, conditions: ['source'] },
  ssr: { resolve: { conditions: ['source'] } },
  test: {
    projects: [
      {
        extends: true,
        test: {
          name: 'web',
          environment: 'jsdom',
          setupFiles: ['./src/test/setup.ts'],
          include: ['src/**/*.test.{ts,tsx}'],
          css: false,
          restoreMocks: true,
        },
      },
      {
        extends: true,
        test: {
          name: 'preview',
          environment: 'node',
          include: ['preview/**/*.test.ts'],
          restoreMocks: true,
        },
      },
    ],
  },
})
