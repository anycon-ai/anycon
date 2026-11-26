// 会话消息里的本地文件链接（docs/spec/web.md § 4.6「本地文件链接」，ADR-62）：agent 写的 `[报告](/Users/…/a.html)`、
// `[x](reports/a.html)`、`file://…`、`C:\…` 指的是节点上的文件，不是 hub 上的地址。这里把它们换成文件浏览的路径
// （`fs.list` / `fs.read` 的 `<项目名>/<相对路径>`），以及目录预览的（目录, URL 路径）。纯函数，不碰 DOM 和网络。
import { resolveLocalUrl, splitPath, treeRoot } from './paths'

/** 解析本地路径需要知道的节点情况。 */
export interface LocalPathContext {
  /** 会话的工作目录（文件浏览里的路径，见 `sessionWorkDir`）；相对路径相对它。没有（项目目录不在了）时相对路径解析不了。 */
  workDir: string | null
  /** 节点上的项目：名字和绝对路径（`workspace.list`，关联项目也在里面）。 */
  projects: ReadonlyArray<{ name: string; path: string }>
  /** 主 workspace 根的绝对路径：scratch 会话的工作目录在它下面的 `.scratch/<会话 id>`。 */
  primaryRoot?: string
}

/** 一个链接地址的归类。 */
export type LocalHref =
  /** 不是本地路径（`https:`、`mailto:`、`//host`）：照普通链接处理。 */
  | { kind: 'remote' }
  /** 只有 `#锚点` / `?查询`：会话消息里没有可去的地方。 */
  | { kind: 'anchor' }
  /** 节点上某个项目（或 scratch 工作目录）里的路径。 */
  | { kind: 'resolved'; path: string }
  /** 是本地路径，但不在任何项目里（比如 `~/Downloads/…`），anycon 打不开。`display` 是给用户复制的路径。 */
  | { kind: 'outside'; display: string }

/** `file:` URL，或 Windows 盘符路径（`C:\…`、`C:/…`；Markdown 解析器会把 `\` 编码成 `%5C`）。 */
export const LOCAL_SCHEME = /^(?:file:|[a-z]:(?:[\\/]|%5c))/i
const DRIVE = /^[a-z]:(?:[\\/]|$)/i
const SCHEME = /^[a-z][a-z0-9+.-]*:/i

function decode(text: string): string {
  try {
    return decodeURIComponent(text)
  } catch {
    // 不是合法的百分号编码：按字面
    return text
  }
}

/** 去掉 `?查询` 和 `#片段`（本地文件名里几乎不会有它们，链接里出现多半是锚点）。 */
function stripSuffix(url: string): string {
  const cut = url.search(/[?#]/)
  return cut === -1 ? url : url.slice(0, cut)
}

/**
 * 绝对路径规整成 `/` 分隔、没有 `.` / `..` 段、没有结尾斜杠的形式。POSIX 路径以 `/` 开头；Windows 路径以 `C:` 开头、
 * 反斜杠换成 `/`。`..` 到了根就停在根（和文件系统一样）。不是绝对路径返回 null。
 */
export function normalizeAbsolute(path: string): string | null {
  if (path.includes('\0')) return null
  const unified = path.replace(/\\/g, '/')
  let head: string
  let rest: string
  if (DRIVE.test(unified)) {
    head = unified.slice(0, 2).toUpperCase()
    rest = unified.slice(2)
  } else if (unified.startsWith('/') && !unified.startsWith('//')) {
    head = ''
    rest = unified
  } else return null
  const out: string[] = []
  for (const seg of rest.split('/')) {
    if (seg === '' || seg === '.') continue
    if (seg === '..') out.pop()
    else out.push(seg)
  }
  return `${head}/${out.join('/')}`
}

/** Windows 的路径不区分大小写：比较用的形式。 */
const compareKey = (abs: string): string => (DRIVE.test(abs) ? abs.toLowerCase() : abs)

/**
 * 节点上的绝对路径 → 文件浏览的路径：落在哪个项目目录里（最长的那个，关联项目可能在别的项目里面）就是 `<项目名>/<剩下的>`；
 * 在主根的 `.scratch/<会话 id>` 里就是 `.scratch/<会话 id>/…`。都不在返回 null。
 */
export function absoluteToFsPath(abs: string, ctx: Pick<LocalPathContext, 'projects' | 'primaryRoot'>): string | null {
  const target = normalizeAbsolute(abs)
  if (target === null) return null
  const key = compareKey(target)
  // 候选：每个项目目录；scratch 的工作目录是 `.scratch/<会话 id>`，至少要有会话 id 那一段
  const candidates: Array<{ name: string; base: string; minRest: number }> = ctx.projects.map((p) => ({ name: p.name, base: p.path, minRest: 0 }))
  if (ctx.primaryRoot) candidates.push({ name: '.scratch', base: `${ctx.primaryRoot.replace(/[\\/]+$/, '')}/.scratch`, minRest: 1 })
  let best: { name: string; prefixLen: number; minRest: number } | null = null
  for (const c of candidates) {
    const norm = normalizeAbsolute(c.base)
    if (norm === null) continue
    const prefix = compareKey(norm)
    const inside = key === prefix || key.startsWith(prefix.endsWith('/') ? prefix : `${prefix}/`)
    if (inside && (best === null || prefix.length > best.prefixLen)) best = { name: c.name, prefixLen: prefix.length, minRest: c.minRest }
  }
  if (best === null) return null
  const rest = splitPath(target.slice(best.prefixLen))
  if (rest.length < best.minRest) return null
  return [best.name, ...rest].join('/')
}

/** `file:` URL → 绝对路径（`file:///C:/x` → `C:/x`）。带主机名的（`file://server/share`，UNC）不认。 */
function fileUrlPath(url: string): string | null {
  let parsed: URL
  try {
    parsed = new URL(url)
  } catch {
    return null
  }
  if (parsed.protocol !== 'file:' || (parsed.host !== '' && parsed.host !== 'localhost')) return null
  const path = decode(parsed.pathname)
  return /^\/[a-z]:/i.test(path) ? path.slice(1) : path
}

/**
 * 会话消息里一个链接 / 图片地址的归类（docs/spec/web.md § 4.6）：
 * - 有别的协议、或 `//` 开头 → `remote`；
 * - `file:` URL、`/` 开头、盘符开头 → 节点上的绝对路径，按项目目录找；
 * - 其余是相对路径，相对会话的工作目录（`..` 不能走出项目目录，`resolveLocalUrl` 的规则）；
 * - `~` 开头的路径不知道 home 在哪，算项目外。
 */
export function classifyLocalHref(href: string, ctx: LocalPathContext): LocalHref {
  if (href === '' || href.startsWith('#') || href.startsWith('?')) return { kind: 'anchor' }
  if (href.startsWith('//')) return { kind: 'remote' }
  if (/^file:/i.test(href)) {
    const abs = fileUrlPath(href)
    if (abs === null) return { kind: 'outside', display: decode(href) }
    const path = absoluteToFsPath(abs, ctx)
    return path === null ? { kind: 'outside', display: abs } : { kind: 'resolved', path }
  }
  const raw = decode(stripSuffix(href))
  if (DRIVE.test(raw) || raw.startsWith('/')) {
    const path = absoluteToFsPath(raw, ctx)
    return path === null ? { kind: 'outside', display: raw } : { kind: 'resolved', path }
  }
  if (SCHEME.test(href)) return { kind: 'remote' }
  if (raw === '~' || raw.startsWith('~/') || raw.startsWith('~\\')) return { kind: 'outside', display: raw }
  // Windows 风格的相对路径（`reports\a.html`，反斜杠可能已被编码成 `%5C`）按 `/` 分隔
  const rel = href.replace(/\\|%5c/gi, '/')
  const path = ctx.workDir === null ? null : resolveLocalUrl(rel, ctx.workDir, treeRoot(ctx.workDir))
  return path === null ? { kind: 'outside', display: raw } : { kind: 'resolved', path }
}

/** 文件名是不是网页（`.html` / `.htm`）：这类文件在目录预览里打开，而不是在文件浏览里看源码。 */
export function isWebPage(path: string): boolean {
  return /\.html?$/i.test(path)
}

/**
 * 文件浏览的路径 → 目录预览的目标（ADR-62）：目录是第一段（项目名，或 `.scratch`），URL 路径是剩下的各段逐段编码，
 * 如 `web/reports/测试报告.html` → `{ dir: 'web', path: '/reports/%E6%B5%8B…' }`。空路径返回 null。
 */
export function previewTargetOf(fsPath: string): { dir: string; path: string } | null {
  const segs = splitPath(fsPath)
  const dir = segs[0]
  if (dir === undefined || dir === '..') return null
  return { dir, path: `/${segs.slice(1).map(encodeURIComponent).join('/')}` }
}
