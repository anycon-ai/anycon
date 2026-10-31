// 文件浏览用的路径和文件类型工具（docs/spec/web.md § 4.7）。纯函数，不碰 DOM 和网络。
// 路径就是 `fs.list` / `fs.read` 的 `path`：`<项目名>/<相对路径>`，用 `/` 分隔，没有前导 `/`。空字符串是「没有选中任何项目」。
import type { FsEntry, SessionWorktree } from '@/protocol'

/** 路径拆成段，去掉空段和 `.`。`..` 原样保留：越界与否由 daemon 判断（`forbidden`），客户端不替它做决定。 */
export function splitPath(path: string): string[] {
  return path.split('/').filter((s) => s !== '' && s !== '.')
}

export function joinPath(...parts: string[]): string {
  return parts.flatMap(splitPath).join('/')
}

/** 上一级；只有一段时是空字符串。 */
export function parentOf(path: string): string {
  return splitPath(path).slice(0, -1).join('/')
}

export function baseName(path: string): string {
  const segs = splitPath(path)
  return segs[segs.length - 1] ?? ''
}

/**
 * 目录树的根：项目名那一段；scratch 会话的工作目录是 `.scratch/<会话 id>`，两段才是一个根。
 * 空路径没有根（页面显示项目列表）。
 */
export function treeRoot(path: string): string {
  const segs = splitPath(path)
  if (segs[0] === '.scratch' && segs.length >= 2) return `${segs[0]}/${segs[1]}`
  return segs[0] ?? ''
}

/** 从 `root` 到 `path` 之间的每一级（含两端），如 `p` → `p/a/b.ts` 得 `[p, p/a, p/a/b.ts]`。`path` 不在 `root` 之下时只有 `path` 自己。 */
export function chain(root: string, path: string): string[] {
  const r = splitPath(root)
  const segs = splitPath(path)
  if (r.length === 0 || r.some((s, i) => segs[i] !== s)) return segs.length ? [segs.join('/')] : []
  return segs.slice(r.length - 1).map((_, i) => segs.slice(0, r.length + i).join('/'))
}

/** 面包屑的每一级：`label` 是这一段的名字，`path` 是走到这里的完整路径。 */
export function crumbsOf(path: string): Array<{ label: string; path: string }> {
  const segs = splitPath(path)
  return segs.map((label, i) => ({ label, path: segs.slice(0, i + 1).join('/') }))
}

export const isHidden = (name: string): boolean => name.startsWith('.')

/** 目录在前，然后按名字（不区分大小写、数字按大小比）；名字相同的按字节序，保证顺序确定。 */
export function sortEntries(entries: FsEntry[]): FsEntry[] {
  return [...entries].sort(
    (a, b) => Number(b.is_dir) - Number(a.is_dir) || a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' }) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0),
  )
}

/**
 * Markdown 里的相对地址（图片 / 链接）→ 节点上的路径。解析不了返回 null：不是相对地址（有协议、`//` 开头）、只有 `#锚点`、
 * 走出了树根（`../..` 超过项目目录）、含 `\` 或 NUL。`/x` 按 GitHub 的习惯相对树根。`?query` 和 `#hash` 去掉，百分号编码解码。
 * 这条路径之后只会交给 `fs.read` / `fs.list`，浏览器不会拿它去发请求。
 */
export function resolveLocalUrl(url: string, fileDir: string, root: string): string | null {
  if (!url || url.startsWith('//') || url.startsWith('#') || /^[a-z][a-z0-9+.-]*:/i.test(url)) return null
  const cut = url.search(/[?#]/)
  const raw = cut === -1 ? url : url.slice(0, cut)
  if (!raw) return null
  let decoded: string
  try {
    decoded = decodeURIComponent(raw)
  } catch {
    return null
  }
  if (decoded.includes('\0') || decoded.includes('\\')) return null
  const rootSegs = splitPath(root)
  const out = decoded.startsWith('/') ? [...rootSegs] : splitPath(fileDir)
  for (const seg of decoded.split('/')) {
    if (seg === '' || seg === '.') continue
    if (seg === '..') {
      if (out.length <= rootSegs.length) return null
      out.pop()
    } else out.push(seg)
  }
  return out.length >= rootSegs.length ? out.join('/') : null
}

export type FileKind = { type: 'image'; mime: string } | { type: 'markdown' } | { type: 'text'; lang?: string }

const IMAGE_MIME: Record<string, string> = {
  png: 'image/png',
  jpg: 'image/jpeg',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
  svg: 'image/svg+xml',
  avif: 'image/avif',
  bmp: 'image/bmp',
  ico: 'image/x-icon',
}

/** 扩展名 → shiki 语言名（`lib/highlight.ts` 里加载得了的）。 */
export const LANG_BY_EXT: Record<string, string> = {
  ts: 'typescript',
  mts: 'typescript',
  cts: 'typescript',
  tsx: 'tsx',
  js: 'javascript',
  mjs: 'javascript',
  cjs: 'javascript',
  jsx: 'jsx',
  json: 'json',
  jsonc: 'json',
  sh: 'bash',
  bash: 'bash',
  zsh: 'bash',
  py: 'python',
  rs: 'rust',
  go: 'go',
  css: 'css',
  scss: 'scss',
  html: 'html',
  htm: 'html',
  yml: 'yaml',
  yaml: 'yaml',
  toml: 'toml',
  diff: 'diff',
  patch: 'diff',
  c: 'c',
  h: 'c',
  cc: 'cpp',
  cpp: 'cpp',
  cxx: 'cpp',
  hpp: 'cpp',
  hh: 'cpp',
  java: 'java',
  kt: 'kotlin',
  kts: 'kotlin',
  swift: 'swift',
  rb: 'ruby',
  sql: 'sql',
  xml: 'xml',
  plist: 'xml',
  lua: 'lua',
  ini: 'ini',
  mk: 'makefile',
}

/** 没有（有用的）扩展名的文件，按完整文件名认。 */
export const LANG_BY_NAME: Record<string, string> = {
  dockerfile: 'dockerfile',
  makefile: 'makefile',
  gnumakefile: 'makefile',
  'cargo.lock': 'toml',
}

/** 文件名 → 怎么预览：图片、Markdown、或文本（带语言时高亮）。 */
export function fileKind(name: string): FileKind {
  const lower = name.toLowerCase()
  const dot = lower.lastIndexOf('.')
  const ext = dot > 0 ? lower.slice(dot + 1) : ''
  const mime = IMAGE_MIME[ext]
  if (mime) return { type: 'image', mime }
  if (ext === 'md' || ext === 'markdown') return { type: 'markdown' }
  const lang = LANG_BY_NAME[lower] ?? LANG_BY_EXT[ext]
  return lang ? { type: 'text', lang } : { type: 'text' }
}

/**
 * 会话的工作目录在文件浏览里的路径（`fs.list` 的 `path`）：worktree 会话是 `<项目>/<worktree 相对路径>`
 * （目录还没建好 / 已经删了就退到项目目录），在项目目录里工作的会话是项目，scratch 会话是 `.scratch/<会话 id>`。
 * 项目不在列表里 / 目录不在了（`projectMeta` 没有或 `missing`）时返回 null：没有能打开的目录。
 */
export function sessionWorkDir(session: { id: string; project?: string | null; worktree?: Pick<SessionWorktree, 'path' | 'state'> | null }, projectMeta: { missing?: boolean } | undefined): string | null {
  if (!session.project) return joinPath('.scratch', session.id)
  if (!projectMeta || projectMeta.missing) return null
  const wt = session.worktree
  return wt && (wt.state === 'active' || wt.state === 'kept') ? joinPath(session.project, wt.path) : joinPath(session.project)
}
