// 预览运行时的构建（spec/web.md § 13.2）：`vite build -c vite.preview.config.ts`，输出 `web/dist-preview`，`base` 是 `/__anycon/`。
//
// 它是预览 origin 上跑的那份前端，和 Web UI（`web/dist`）分开构建、分开嵌入、互不包含对方的文件
// （Web UI 的 origin 上取不到预览运行时，否则可能被诱导去注册一个隧道到别人节点的 service worker）。产物：
//
// - `index.html` + `assets/*`：引导页（没有内联脚本，hub 的 CSP 是 `script-src 'self' 'wasm-unsafe-eval'`）；
// - `sw.js`、`shim.js`：文件名固定、不带哈希的单文件经典脚本（不含 `import` / `import.meta`；Firefox 对 module service worker
//   的支持晚）。它们不走 HTML 入口，所以由下面的插件在主构建结束后各自再跑一次 IIFE 构建；
// - `assets/anycon_crypto_bg-<hash>.wasm`：Noise wasm（`just gen-wasm` 的产物）。`sw.js` 按这个 URL 显式加载它；
//   wasm 还没构建时构建照样能过，运行时 service worker 会报「加密模块没能加载」。
import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath, URL } from 'node:url'
import { build, defaultClientConditions, defineConfig, type InlineConfig, type Plugin } from 'vite'
import { thirdPartyLicenses } from './scripts/third-party-licenses'

const root = fileURLToPath(new URL('./preview', import.meta.url))
const outDir = fileURLToPath(new URL('./dist-preview', import.meta.url))
const wasmFile = process.env.ANYCON_WASM_JS
  ? process.env.ANYCON_WASM_JS.replace(/anycon_crypto\.js$/, 'anycon_crypto_bg.wasm')
  : fileURLToPath(new URL('../packages/protocol/wasm/anycon_crypto_bg.wasm', import.meta.url))
const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')) as { version: string }
const BASE = '/__anycon/'

/** 和 `vite.config.ts` 一样：wasm 胶水 js 不在时换成运行时才报错的桩，让 typecheck / test / build 仍然能过。 */
function wasmFallback(): Plugin {
  const real = process.env.ANYCON_WASM_JS ?? fileURLToPath(new URL('../packages/protocol/wasm/anycon_crypto.js', import.meta.url))
  const stub = '\0anycon-wasm-missing'
  return {
    name: 'anycon-preview-wasm-fallback',
    enforce: 'pre',
    resolveId(source) {
      if (/(^|\/)anycon_crypto\.js$/.test(source) && !existsSync(real)) {
        this.warn('anycon crypto wasm is not built (packages/protocol/wasm); the preview service worker will fail to start tunnels. Run `just gen-wasm`.')
        return stub
      }
    },
    load(id) {
      if (id === stub) return 'export default async function init() { throw new Error("anycon crypto wasm is not built: run `just gen-wasm`") }'
    },
  }
}

/**
 * `@anycon/protocol` 的 `crypto.ts` 和 wasm 胶水里有只在 Node / 无参数默认路径上用到的写法，放进经典脚本会出问题：
 * `import.meta.url`（经典脚本里是语法错误）和读 `node:fs` 的动态 `import()`（service worker 里 `import()` 直接被禁）。
 * 这些路径在预览里永远走不到（wasm 的 URL 是显式给的），换成等价的无害写法。
 */
function classicScriptFixups(): Plugin {
  const targets = /\/packages\/protocol\/(src\/crypto\.ts|wasm\/anycon_crypto\.js)$/
  return {
    name: 'anycon-preview-classic-fixups',
    enforce: 'pre',
    transform(code, id) {
      if (!targets.test(id.split('?')[0]!.replaceAll('\\', '/'))) return
      return code
        .replaceAll('import.meta.url', 'self.location.href')
        .replace(/import\(\s*\/\*\s*@vite-ignore\s*\*\/\s*fsSpecifier\s*\)/g, 'Promise.reject(new Error("node:fs is not available in the browser"))')
    },
  }
}

/** 兜底检查：`sw.js` / `shim.js` 里不能留下 `import.meta`、`import()` 和模块语法，否则浏览器注册 / 执行时才发现。 */
function classicScriptGuard(): Plugin {
  return {
    name: 'anycon-preview-classic-guard',
    renderChunk(code, chunk) {
      const problems = [
        /\bimport\.meta\b/.test(code) && 'import.meta',
        /\bimport\s*\(/.test(code) && 'import()',
        /(^|[;}\n])\s*(import|export)\s*[{*"'\w]/.test(code) && 'module syntax',
      ].filter(Boolean)
      if (problems.length > 0) this.error(`${chunk.fileName} must be a classic script, but it contains ${problems.join(', ')}`)
      return null
    },
  }
}

function classicBuild(entry: string, fileName: string, name: string, define: Record<string, string>): InlineConfig {
  return {
    configFile: false,
    root,
    base: BASE,
    publicDir: false,
    logLevel: 'warn',
    define,
    resolve: { conditions: ['source', ...defaultClientConditions] },
    plugins: [classicScriptFixups(), wasmFallback(), classicScriptGuard()],
    build: {
      outDir,
      emptyOutDir: false,
      copyPublicDir: false,
      target: 'es2022',
      sourcemap: false,
      lib: { entry: resolve(root, entry), formats: ['iife'], name, fileName: () => fileName },
    },
  }
}

/** 主构建（引导页）收尾时再构建 `sw.js` 和 `shim.js`，并把 Noise wasm 放进 `assets/`。 */
function previewRuntime(): Plugin {
  let wasmPath = ''
  return {
    name: 'anycon-preview-runtime',
    apply: 'build',
    generateBundle() {
      if (!existsSync(wasmFile)) {
        this.warn(`Noise wasm not found at ${wasmFile}; the preview runtime will report that the encryption module is missing. Run \`just gen-wasm\`.`)
        return
      }
      const ref = this.emitFile({ type: 'asset', name: 'anycon_crypto_bg.wasm', source: readFileSync(wasmFile) })
      wasmPath = this.getFileName(ref)
    },
    async closeBundle() {
      const define = {
        __PREVIEW_WASM_URL__: JSON.stringify(wasmPath ? `${BASE}${wasmPath}` : ''),
        __PREVIEW_VERSION__: JSON.stringify(pkg.version),
      }
      await build(classicBuild('src/sw.ts', 'sw.js', 'AnyconPreviewWorker', define))
      await build(classicBuild('src/shim.ts', 'shim.js', 'AnyconPreviewShim', define))
    },
  }
}

export default defineConfig({
  root,
  base: BASE,
  publicDir: false,
  resolve: {
    // workspace 包 @anycon/protocol 直接用 TS 源码（不依赖它的 dist 构建产物）
    conditions: ['source', ...defaultClientConditions],
  },
  define: { __PREVIEW_WASM_URL__: '""', __PREVIEW_VERSION__: JSON.stringify(pkg.version) },
  plugins: [
    classicScriptFixups(),
    wasmFallback(),
    // dist-preview/THIRD-PARTY-LICENSES.txt：sw.js 里打进了 @msgpack/msgpack（引导页的模块图里不一定有它，所以显式列出）
    thirdPartyLicenses({ extra: ['@msgpack/msgpack'] }),
    previewRuntime(),
  ],
  build: {
    outDir,
    emptyOutDir: true,
    target: 'es2022',
    sourcemap: false,
  },
})
