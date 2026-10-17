import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vitest/config'

// 测试用单独的配置：不加载路由生成 / Tailwind / PWA 插件，tsx 交给 vitest 自带的转换。
export default defineConfig({
  define: { __APP_VERSION__: JSON.stringify('0.0.0-test') },
  resolve: { alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) }, conditions: ['source'] },
  ssr: { resolve: { conditions: ['source'] } },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.{ts,tsx}'],
    css: false,
    restoreMocks: true,
  },
})
