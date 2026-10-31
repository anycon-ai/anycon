// shiki：只加载一小组语言，按需懒加载；双主题输出（CSS 变量切换明暗，见 index.css）。
import type { HighlighterCore } from 'shiki/core'

const LANG_LOADERS: Record<string, () => Promise<{ default: unknown }>> = {
  typescript: () => import('shiki/langs/typescript.mjs'),
  tsx: () => import('shiki/langs/tsx.mjs'),
  javascript: () => import('shiki/langs/javascript.mjs'),
  jsx: () => import('shiki/langs/jsx.mjs'),
  json: () => import('shiki/langs/json.mjs'),
  bash: () => import('shiki/langs/bash.mjs'),
  python: () => import('shiki/langs/python.mjs'),
  rust: () => import('shiki/langs/rust.mjs'),
  go: () => import('shiki/langs/go.mjs'),
  css: () => import('shiki/langs/css.mjs'),
  html: () => import('shiki/langs/html.mjs'),
  yaml: () => import('shiki/langs/yaml.mjs'),
  toml: () => import('shiki/langs/toml.mjs'),
  markdown: () => import('shiki/langs/markdown.mjs'),
  diff: () => import('shiki/langs/diff.mjs'),
  // 文件浏览（docs/spec/web.md § 4.7）按扩展名选语言：再加几种常见的，都是按需懒加载的小块
  c: () => import('shiki/langs/c.mjs'),
  cpp: () => import('shiki/langs/cpp.mjs'),
  java: () => import('shiki/langs/java.mjs'),
  kotlin: () => import('shiki/langs/kotlin.mjs'),
  swift: () => import('shiki/langs/swift.mjs'),
  ruby: () => import('shiki/langs/ruby.mjs'),
  sql: () => import('shiki/langs/sql.mjs'),
  xml: () => import('shiki/langs/xml.mjs'),
  scss: () => import('shiki/langs/scss.mjs'),
  lua: () => import('shiki/langs/lua.mjs'),
  dockerfile: () => import('shiki/langs/dockerfile.mjs'),
  makefile: () => import('shiki/langs/makefile.mjs'),
  ini: () => import('shiki/langs/ini.mjs'),
}

const ALIASES: Record<string, string> = {
  ts: 'typescript',
  js: 'javascript',
  mjs: 'javascript',
  cjs: 'javascript',
  sh: 'bash',
  shell: 'bash',
  zsh: 'bash',
  console: 'bash',
  py: 'python',
  rs: 'rust',
  yml: 'yaml',
  md: 'markdown',
  patch: 'diff',
  'c++': 'cpp',
  kt: 'kotlin',
  rb: 'ruby',
  docker: 'dockerfile',
  make: 'makefile',
}

export function resolveLang(lang: string | undefined): string | null {
  if (!lang) return null
  const l = lang.toLowerCase()
  const name = ALIASES[l] ?? l
  return name in LANG_LOADERS ? name : null
}

let highlighter: Promise<HighlighterCore> | null = null
const loaded = new Set<string>()

async function getHighlighter(): Promise<HighlighterCore> {
  highlighter ??= (async () => {
    const [{ createHighlighterCore }, { createJavaScriptRegexEngine }] = await Promise.all([import('shiki/core'), import('shiki/engine/javascript')])
    return createHighlighterCore({
      themes: [import('shiki/themes/github-light.mjs'), import('shiki/themes/dark-plus.mjs')],
      langs: [],
      engine: createJavaScriptRegexEngine(),
    })
  })()
  return highlighter
}

/** 返回高亮后的 HTML（`<pre class="shiki">…`）；语言不支持或失败时返回 null（调用方显示纯文本）。 */
export async function highlightCode(code: string, lang: string | undefined): Promise<string | null> {
  const name = resolveLang(lang)
  if (!name) return null
  try {
    const hl = await getHighlighter()
    if (!loaded.has(name)) {
      const mod = await LANG_LOADERS[name]!()
      await hl.loadLanguage(mod.default as Parameters<HighlighterCore['loadLanguage']>[0])
      loaded.add(name)
    }
    return hl.codeToHtml(code, { lang: name, themes: { light: 'github-light', dark: 'dark-plus' }, defaultColor: false })
  } catch {
    return null
  }
}
