// 品牌图标生成器：从 public/logo.svg（彩色主标）渲染 public/ 下的各尺寸 PNG。
//
//   node web/scripts/icons.mjs
//
// SVG 源文件（logo.svg、favicon.svg、logo-mono-*.svg）是设计稿原件，直接放在 public/ 里，不由脚本生成；
// 这里只负责 PWA / iOS 需要的位图。`src/components/Logo.tsx` 内联的是同一条外形路径，测试会比对。
// PNG 用 e2e 里装好的 Playwright Chromium 渲染（根目录 `bun install` 后即可用），不引入新依赖。
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'

const PUBLIC = fileURLToPath(new URL('../public/', import.meta.url))
const SOURCE = readFileSync(PUBLIC + 'logo.svg', 'utf8')
const TILE = '#ffffff' // 铺满型图标的底色：彩色主标在白底上对比最好

// 取出根 <svg> 里面的内容（defs + 图形），好包进带底色、带缩放的新画布
const inner = SOURCE.slice(SOURCE.indexOf('>', SOURCE.indexOf('<svg')) + 1, SOURCE.lastIndexOf('</svg>'))

/**
 * 组一张 256×256 的 SVG。`tile` 为真时铺白底（maskable / apple-touch 不能透明），
 * `scale` 把主标绕中心缩小，让它落进系统的安全区。
 */
function svg({ tile = false, scale = 1 } = {}) {
  const bg = tile ? `<rect width="256" height="256" fill="${TILE}"/>` : ''
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">${bg}` +
    `<g transform="translate(128 128) scale(${scale}) translate(-128 -128)">${inner}</g></svg>`
  )
}

// 输出清单。主标的外接框约占画布的 84%（x 23–238、y 25–232），角上的两只脚离中心最远
const OUTPUTS = [
  // purpose: any —— 透明底，原样
  { file: 'icon-192.png', size: 192, svg: svg() },
  { file: 'icon-512.png', size: 512, svg: svg() },
  // maskable：安全区是直径 80% 的圆；两只脚到中心约 0.56 × 主标尺寸，缩到 0.68 才整个落在圆里
  { file: 'icon-maskable-512.png', size: 512, svg: svg({ tile: true, scale: 0.68 }) },
  // apple-touch：铺满、不带圆角（iOS 自己套），留约 10% 边
  { file: 'apple-touch-icon.png', size: 180, svg: svg({ tile: true, scale: 0.8 }) },
]

const { chromium } = createRequire(new URL('../../e2e/package.json', import.meta.url))('@playwright/test')
const browser = await chromium.launch()
try {
  const page = await browser.newPage({ viewport: { width: 512, height: 512 }, deviceScaleFactor: 1 })
  for (const o of OUTPUTS) {
    await page.setContent(`<body style="margin:0;background:transparent"><img width="${o.size}" height="${o.size}" src="data:image/svg+xml;base64,${Buffer.from(o.svg).toString('base64')}"></body>`)
    await page.locator('img').evaluate((img) => img.decode())
    await page.screenshot({ path: PUBLIC + o.file, omitBackground: true, clip: { x: 0, y: 0, width: o.size, height: o.size } })
  }
} finally {
  await browser.close()
}
console.log('wrote', OUTPUTS.map((o) => o.file).join(', '), 'to', PUBLIC)
