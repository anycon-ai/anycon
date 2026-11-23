/**
 * 界面文案（`web/src/i18n/{en,zh}.json`）的断言辅助：用例按键名找文案，英文和中文各一份、整串精确匹配，
 * 所以不在用例里重抄文案，界面改了措辞也只改 JSON。只用于没有 `data-testid` 的按钮、输入框和说明。
 */
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

import { repoRoot } from './env.js'

type Tree = { [key: string]: string | Tree }

const catalogs: Tree[] = ['en', 'zh'].map(
  (lang) =>
    JSON.parse(readFileSync(join(repoRoot, 'web', 'src', 'i18n', `${lang}.json`), 'utf8')) as Tree,
)

function lookup(tree: Tree, key: string): string | undefined {
  let node: string | Tree | undefined = tree
  for (const part of key.split('.')) {
    if (typeof node !== 'object') return undefined
    node = node[part]
  }
  return typeof node === 'string' ? node : undefined
}

const escape = (s: string): string => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')

/** 某个文案键的各语言文字（没有插值的键）。 */
export function texts(key: string): string[] {
  const found = catalogs.map((c) => lookup(c, key)).filter((v): v is string => v !== undefined)
  if (!found.length) throw new Error(`no i18n text for ${key}`)
  return found
}

/** 把文案里的 `{{变量}}` 换成给定的值（没给的变量匹配任意文字），其余原样转义。 */
function pattern(text: string, vars: Record<string, string>): string {
  return text
    .split(/(\{\{\w+\}\})/)
    .map((part) => {
      const m = /^\{\{(\w+)\}\}$/.exec(part)
      if (!m) return escape(part)
      const value = vars[m[1]!]
      return value === undefined ? '.+' : escape(value)
    })
    .join('')
}

/** 整串精确匹配某个键的文案（英文或中文）；带插值的文案用 `vars` 给值。 */
export function exactText(key: string, vars: Record<string, string> = {}): RegExp {
  return new RegExp(
    `^(${texts(key)
      .map((t) => pattern(t, vars))
      .join('|')})$`,
  )
}

/** 文案里含有某个键的文字（用于带插值的句子里不变的一段）。 */
export function containsText(key: string, vars: Record<string, string> = {}): RegExp {
  return new RegExp(
    `(${texts(key)
      .map((t) => pattern(t, vars))
      .join('|')})`,
  )
}
