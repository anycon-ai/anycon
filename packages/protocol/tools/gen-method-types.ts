/**
 * 从 `schema/methods.json`（`crates/protocol::methods::REGISTRY` 导出）生成 `src/method-types.ts`。
 *
 * 用法（在仓库根，`just gen-ts` 会跑）：
 *   bun packages/protocol/tools/gen-method-types.ts            # 写 src/method-types.ts
 *   bun packages/protocol/tools/gen-method-types.ts --check    # 不写，和已提交的文件不一致则退出码 1
 *
 * 类型名必须是 `src/generated.ts` 导出的名字（`schema/protocol.json` 生成）；找不到就报错，
 * 而不是生成一个编译不过的文件。输出已按 `.oxfmtrc.json` 的风格排版（宽度 100、单引号、无分号），
 * 所以 `oxfmt --check` 不会对它报差异。
 */
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath, pathToFileURL } from 'node:url'

/** `schema/methods.json` 的一项，对应 Rust 的 `MethodSpec`。 */
export interface MethodSpec {
  name: string
  direction: 'request' | 'notification'
  /** 类型名；`null` = 无参数（`Empty`）。 */
  params: string | null
  /** 类型名；`null` = 无结果（`Empty`）。 */
  result: string | null
}

const WIDTH = 100
const EMPTY = 'Empty'

const HEADER = `/**
 * 方法名 → params / result 的类型表，供 \`RpcPeer.request\` 之上的类型化封装用。
 *
 * 由 \`just gen-ts\` 从 \`schema/methods.json\`（\`crates/protocol/src/methods.rs\` 的 \`REGISTRY\`）生成，
 * 不要手改；\`tests/method-types.test.ts\` 会断言它与 schema 同步。类型名来自 \`generated.ts\`。
 * 没有参数 / 没有结果的方法记作 \`Empty\`（线上是 \`{}\`）。
 */
`

/** `generated.ts` 里导出的类型名（interface / type）。 */
export function exportedTypeNames(generatedSource: string): Set<string> {
  const names = new Set<string>()
  for (const m of generatedSource.matchAll(/^export (?:interface|type) ([A-Za-z0-9_]+)/gm)) {
    names.add(m[1]!)
  }
  return names
}

function member(name: string, fields: Array<[string, string]>): string {
  const inline = `  '${name}': { ${fields.map(([k, v]) => `${k}: ${v}`).join('; ')} }`
  if (inline.length <= WIDTH) return inline
  return [`  '${name}': {`, ...fields.map(([k, v]) => `    ${k}: ${v}`), '  }'].join('\n')
}

/** 纯函数：registry + generated.ts 的源码 → method-types.ts 的全文。 */
export function renderMethodTypes(methods: readonly MethodSpec[], generatedSource: string): string {
  const known = exportedTypeNames(generatedSource)
  if (!known.has(EMPTY)) throw new Error(`generated.ts does not export ${EMPTY}`)

  const seen = new Set<string>()
  const used = new Set<string>([EMPTY])
  for (const m of methods) {
    if (seen.has(m.name)) throw new Error(`duplicate method in registry: ${m.name}`)
    seen.add(m.name)
    for (const type of [m.params, m.result]) {
      if (type === null) continue
      if (!known.has(type)) {
        throw new Error(`method ${m.name}: type ${type} is not exported by generated.ts`)
      }
      used.add(type)
    }
  }

  const names = [...used].toSorted()
  const importLine = `import type { ${names.join(', ')} } from './generated.js'`
  const imports =
    importLine.length <= WIDTH
      ? importLine
      : `import type {\n${names.map((n) => `  ${n},`).join('\n')}\n} from './generated.js'`

  const requests = methods.filter((m) => m.direction === 'request')
  const notifications = methods.filter((m) => m.direction === 'notification')

  const out = [
    HEADER.trimEnd(),
    imports,
    '',
    '/** C→D 请求方法。 */',
    'export interface RequestMethods {',
    requests
      .map((m) =>
        member(m.name, [
          ['params', m.params ?? EMPTY],
          ['result', m.result ?? EMPTY],
        ]),
      )
      .join('\n'),
    '}',
    '',
    '/** D→C 通知方法（没有响应）。 */',
    'export interface NotificationMethods {',
    notifications.map((m) => member(m.name, [['params', m.params ?? EMPTY]])).join('\n'),
    '}',
    '',
    'export type RequestMethodName = keyof RequestMethods',
    'export type NotificationMethodName = keyof NotificationMethods',
    '',
  ]
  return out.join('\n')
}

const packageDir = fileURLToPath(new URL('..', import.meta.url))
export const METHODS_JSON = fileURLToPath(new URL('../../../schema/methods.json', import.meta.url))
export const GENERATED_TS = `${packageDir}src/generated.ts`
export const METHOD_TYPES_TS = `${packageDir}src/method-types.ts`

/** 读磁盘上的 schema 和 generated.ts，渲染出应有的 method-types.ts。 */
export function renderFromDisk(): string {
  const methods = JSON.parse(readFileSync(METHODS_JSON, 'utf8')) as MethodSpec[]
  return renderMethodTypes(methods, readFileSync(GENERATED_TS, 'utf8'))
}

function main(): void {
  const expected = renderFromDisk()
  if (process.argv.includes('--check')) {
    const actual = readFileSync(METHOD_TYPES_TS, 'utf8')
    if (actual !== expected) {
      console.error('packages/protocol/src/method-types.ts is out of date: run `just gen-ts`')
      process.exit(1)
    }
    return
  }
  writeFileSync(METHOD_TYPES_TS, expected)
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main()
