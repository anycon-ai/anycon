// 分支名的基本校验和显示用的小函数。真正的校验在 daemon（`git check-ref-format --branch`），这里只挡明显写错的，
// 让用户在发送之前就看到问题。纯函数。

export type BranchNameProblem = 'invalid' | 'reserved'

/** daemon 不接受这几个名字作为新分支（daemon.md § 10.1「创建」）。 */
const RESERVED = new Set(['main', 'master', 'HEAD'])

/**
 * 按 git 的分支名规则做基本校验（空串不算问题：留空由 daemon 生成）。
 * 规则：不含控制字符、空格和 `~ ^ : ? * [ \`；不含 `..` 和 `@{`；不以 `-`、`/` 开头，不以 `/`、`.` 结尾，没有 `//`；
 * 每一段不以 `.` 开头、不以 `.lock` 结尾；不是 `@`。
 */
export function branchNameProblem(raw: string): BranchNameProblem | null {
  const name = raw.trim()
  if (name === '') return null
  if (RESERVED.has(name)) return 'reserved'
  if (name.length > 255 || name === '@' || name.startsWith('-') || name.startsWith('/') || name.endsWith('/') || name.endsWith('.')) return 'invalid'
  // eslint-disable-next-line no-control-regex
  if (/[\u0000- \u007f~^:?*[\\]/.test(name)) return 'invalid'
  if (name.includes('..') || name.includes('@{') || name.includes('//')) return 'invalid'
  for (const part of name.split('/')) {
    if (part.startsWith('.') || part.endsWith('.lock')) return 'invalid'
  }
  return null
}

/** 输入框的占位符：`anycon/…`；节点配置的前缀为空时只说「自动」。 */
export function branchPlaceholder(prefix: string | undefined, auto: string): string {
  return prefix ? `${prefix}/${auto}` : auto
}

/**
 * `base` 是不是一个提交而不是分支：daemon 给的展示名在 base 是提交时是它的前 12 位十六进制
 * （`--base HEAD` / 标签以外的 SHA 写法），所以十六进制且是 `base_sha` 的前缀就算。标签（`v1.0`）、分支名都不是。
 */
export function isCommitBase(base: string, baseSha: string | null | undefined): boolean {
  return /^[0-9a-f]{4,40}$/i.test(base) && !!baseSha && baseSha.toLowerCase().startsWith(base.toLowerCase())
}

/** PR 的目标分支名：把 `origin/main` 这样的远端跟踪分支去掉 `origin/`（`gh pr create --base` 要的是分支名）。 */
export function prBaseBranch(base: string): string {
  return base.startsWith('origin/') ? base.slice('origin/'.length) : base
}
