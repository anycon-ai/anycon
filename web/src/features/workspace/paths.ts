// 目录选择器用的纯函数：目录名、默认项目名、项目名校验、面包屑。
// 路径来自 daemon 所在机器：可能是 `/` 分隔（macOS / Linux），也可能是 `\` 分隔（Windows），这里两种都认。

/** 路径分隔符：路径里有 `\` 而没有 `/` 才当作 Windows 路径。 */
export function sepOf(path: string): '/' | '\\' {
  return path.includes('\\') && !path.includes('/') ? '\\' : '/'
}

/** 路径的最后一段（目录名）。根目录（`/`、`C:\`）返回空串。 */
export function baseName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, '')
  const i = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'))
  const last = trimmed.slice(i + 1)
  // `C:` 这种盘符不是目录名
  return /^[A-Za-z]:$/.test(last) ? '' : last
}

/** 选中目录后的默认项目名：目录名；以 `.` 开头的目录名（项目名不允许）去掉前导的点。 */
export function defaultProjectName(path: string): string {
  return baseName(path).replace(/^\.+/, '')
}

export type NameProblem = 'empty' | 'invalid' | 'too_long'

const encoder = new TextEncoder()

/** C0 / DEL / C1 控制字符（daemon 同样拒绝）。 */
function hasControlChar(s: string): boolean {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i)
    if (c <= 0x1f || (c >= 0x7f && c <= 0x9f)) return true
  }
  return false
}

/**
 * 项目名校验，规则同 daemon（spec/daemon.md § 10）：不含 `/`、`\` 和控制字符（含 NUL）、不以 `.` 开头、≤ 255 字节。
 * 返回 null 表示没问题；重名另用 `nameClash` 对着项目列表查。
 */
export function projectNameProblem(raw: string): NameProblem | null {
  const name = raw.trim()
  if (!name) return 'empty'
  if (name.includes('/') || name.includes('\\') || hasControlChar(name) || name.startsWith('.')) return 'invalid'
  if (encoder.encode(name).length > 255) return 'too_long'
  return null
}

/** 比较项目名用的形式：NFC（`Café` 的两种写法算同一个名字）。 */
export function nfcName(name: string): string {
  return name.trim().normalize('NFC')
}

/** macOS / Windows 的文件系统默认不区分大小写，daemon 在那里按不区分大小写比较项目名。 */
export function caseInsensitiveHost(platform: string | undefined): boolean {
  return /^(macos|darwin|windows)/i.test(platform ?? '')
}

/**
 * 新名字和已有项目名的冲突（和 daemon 一致：NFC 之后比较，不区分大小写的文件系统上再忽略大小写）：
 * `exact` = NFC 之后完全一样，一定会被拒绝；`case` = 只有大小写不同（是否算重名取决于那台机器的文件系统）。
 * 返回冲突的那个已有名字。
 */
export function nameClash(taken: readonly string[], name: string): { kind: 'exact' | 'case'; with: string } | null {
  const want = nfcName(name)
  let loose: string | null = null
  for (const t of taken) {
    const have = nfcName(t)
    if (have === want) return { kind: 'exact', with: t }
    if (loose === null && have.toLowerCase() === want.toLowerCase()) loose = t
  }
  return loose === null ? null : { kind: 'case', with: loose }
}

export interface Crumb {
  label: string
  path: string
}

/**
 * 面包屑：从 `path` 所在的 root 开始（不能走出 root），root 本身是第一段。
 * 多个 root 嵌套时取最长（最近）的那个。`path` 不在任何 root 里（不应发生）就只给一段完整路径。
 */
export function crumbsFor(path: string, roots: readonly string[]): Crumb[] {
  const sep = sepOf(path)
  const strip = (p: string): string => (p.length > 1 && !/^[A-Za-z]:[\\/]$/.test(p) ? p.replace(/[\\/]+$/, '') || p.slice(0, 1) : p)
  const here = strip(path)
  let best: string | null = null
  for (const r of roots) {
    const root = strip(r)
    const prefix = root.endsWith(sep) ? root : root + sep
    if ((here === root || here.startsWith(prefix)) && (best === null || root.length > best.length)) best = root
  }
  if (best === null) return [{ label: path, path }]
  const rest = here === best ? '' : here.slice(best.endsWith(sep) ? best.length : best.length + 1)
  const crumbs: Crumb[] = [{ label: baseName(best) || best, path: best }]
  let acc = best
  for (const seg of rest.split(/[\\/]+/).filter(Boolean)) {
    acc = acc.endsWith(sep) ? acc + seg : acc + sep + seg
    crumbs.push({ label: seg, path: acc })
  }
  return crumbs
}
