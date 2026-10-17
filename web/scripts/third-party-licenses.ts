// Vite 插件：构建时把打进 Web UI 的 npm 包的许可证收集成 dist 根下的 `THIRD-PARTY-LICENSES.txt`。
//
// 这份文件随 Web UI 一起嵌进 hub、由 hub 下发（浏览器拿到的是这些包的代码，许可证也应该能拿到），
// hub 镜像里另存一份（docker/hub.Dockerfile）。Rust 侧的清单由 cargo-about 生成（packaging/about.toml）。
//
// 包的来源：主构建里真正进了 chunk 的模块（rollup 的 `chunk.modules`），加上 `extra` 列出的、不在主构建模块图里的包
// （service worker 是 vite-plugin-pwa 单独构建的；CSS 里有 Tailwind 的 preflight），连同它们的运行时依赖。
// workspace 包（@anycon/protocol 等）不在 node_modules 里，不计入。
import { existsSync, readdirSync, readFileSync, realpathSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import type { Plugin } from 'vite'

const FILE_NAME = 'THIRD-PARTY-LICENSES.txt'
const LICENSE_FILE = /^(licen[cs]e|copying|notice)([.-]|$)/i

interface PackageJson {
  name?: string
  version?: string
  license?: string | { type?: string }
  repository?: string | { url?: string }
  dependencies?: Record<string, string>
}

function readPackageJson(root: string): PackageJson {
  return JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')) as PackageJson
}

/** 模块路径 → 所属包的根目录（最后一个 `node_modules/` 之后的 `name` 或 `@scope/name`）。 */
function packageRootOf(id: string): string | undefined {
  const path = id.replace(/^\0/, '').split('?')[0] ?? ''
  const at = path.lastIndexOf('/node_modules/')
  if (at < 0) return undefined
  const rest = path.slice(at + '/node_modules/'.length).split('/')
  const name = rest[0]?.startsWith('@') ? rest.slice(0, 2).join('/') : rest[0]
  return name ? path.slice(0, at + '/node_modules/'.length) + name : undefined
}

/** 按 Node 的查找规则从 `fromDir` 往上找 `node_modules/<name>`（hoisted 和 bun 的 isolated 布局都适用）。 */
function findPackage(fromDir: string, name: string): string | undefined {
  for (let dir = fromDir; ; dir = dirname(dir)) {
    const candidate = join(dir, 'node_modules', name)
    if (existsSync(join(candidate, 'package.json'))) return realpathSync(candidate)
    if (dir === dirname(dir)) return undefined
  }
}

function addWithDependencies(roots: Set<string>, root: string): void {
  if (roots.has(root)) return
  roots.add(root)
  for (const dep of Object.keys(readPackageJson(root).dependencies ?? {})) {
    const found = findPackage(root, dep)
    if (found) addWithDependencies(roots, found)
  }
}

function render(roots: Set<string>): string {
  const entries = [...roots].map((root) => {
    const pkg = readPackageJson(root)
    const license = typeof pkg.license === 'string' ? pkg.license : (pkg.license?.type ?? 'UNKNOWN')
    const repo = typeof pkg.repository === 'string' ? pkg.repository : pkg.repository?.url
    const texts = readdirSync(root)
      .filter((f) => LICENSE_FILE.test(f))
      .sort()
      .map((f) => readFileSync(join(root, f), 'utf8').trim())
    return { id: `${pkg.name ?? root}@${pkg.version ?? '?'}`, license, repo, texts }
  })
  entries.sort((a, b) => a.id.localeCompare(b.id))
  const header = [
    'THIRD-PARTY SOFTWARE NOTICES (Web UI)',
    '=====================================',
    '',
    'The Anycon Web UI bundles the npm packages listed below. Each section names a',
    'package, its declared license, and the license text shipped with the package.',
    '',
  ]
  const sections = entries.map((e) =>
    [
      '-'.repeat(80),
      `${e.id} (${e.license})${e.repo ? ` <${e.repo}>` : ''}`,
      '',
      e.texts.length > 0 ? e.texts.join('\n\n') : `(the package ships no license file; declared license: ${e.license})`,
      '',
    ].join('\n'),
  )
  return [...header, ...sections].join('\n')
}

/** `extra`：不在主构建模块图里、但会被打进产物的包名（从 web/ 目录解析）。 */
export function thirdPartyLicenses({ extra = [] }: { extra?: string[] } = {}): Plugin {
  return {
    name: 'anycon-third-party-licenses',
    apply: 'build',
    generateBundle(_options, bundle) {
      const roots = new Set<string>()
      for (const output of Object.values(bundle)) {
        if (output.type !== 'chunk') continue
        for (const [id, mod] of Object.entries(output.modules)) {
          // 整个被 tree-shake 掉的模块不算
          if (mod.renderedLength === 0) continue
          const root = packageRootOf(id)
          if (root) roots.add(realpathSync(root))
        }
      }
      const webDir = fileURLToPath(new URL('..', import.meta.url))
      for (const name of extra) {
        const root = findPackage(webDir, name)
        if (!root) this.error(`third-party licenses: package ${name} not found from ${webDir}`)
        addWithDependencies(roots, root)
      }
      this.emitFile({ type: 'asset', fileName: FILE_NAME, source: render(roots) })
    },
  }
}
