// method-types.ts 是生成物：必须和 schema/methods.json 同步（`just gen-ts`），且方法名常量与注册表一一对应。
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { describe, it } from 'node:test'

import {
  GENERATED_TS,
  METHODS_JSON,
  METHOD_TYPES_TS,
  exportedTypeNames,
  renderFromDisk,
  renderMethodTypes,
  type MethodSpec,
} from '../tools/gen-method-types.js'
import { METHODS } from '../src/index.js'
import { repoRoot } from './helpers.js'

const methods = JSON.parse(readFileSync(METHODS_JSON, 'utf8')) as MethodSpec[]
const generated = readFileSync(GENERATED_TS, 'utf8')
const resultOf = (name: string) => methods.find((m) => m.name === name)?.result

describe('method-types.ts 由 schema/methods.json 生成', () => {
  it('已提交的文件与生成器输出逐字节一致（不一致就跑 `just gen-ts`）', () => {
    assert.equal(readFileSync(METHOD_TYPES_TS, 'utf8'), renderFromDisk())
  })

  it('schema/methods.json 与 schema/protocol.json 的 x-anycon-methods 一致', () => {
    const protocol = JSON.parse(readFileSync(`${repoRoot}schema/protocol.json`, 'utf8')) as Record<
      string,
      unknown
    >
    assert.deepEqual(protocol['x-anycon-methods'], methods)
  })

  it('注册表里的类型名都是 generated.ts 导出的', () => {
    const known = exportedTypeNames(generated)
    for (const m of methods) {
      for (const t of [m.params, m.result])
        if (t !== null) assert.ok(known.has(t), `${m.name}: ${t}`)
    }
  })

  it('METHODS 常量与注册表的方法名集合相同', () => {
    assert.deepEqual(Object.values(METHODS).toSorted(), methods.map((m) => m.name).toSorted())
  })

  it('通知没有 result，请求方向与 METHODS 注释一致', () => {
    for (const m of methods) {
      if (m.direction === 'notification') {
        assert.equal(m.result, null, m.name)
        assert.notEqual(m.params, null, m.name)
      }
    }
    assert.ok(methods.some((m) => m.name === 'pair.ack' && m.direction === 'notification'))
  })

  it('返回类型来自注册表，与 daemon 的实际返回一致（不是手写的）', () => {
    assert.equal(resultOf('workspace.rename'), 'RenamedProject')
    assert.equal(resultOf('workspace.trash'), 'TrashEntry')
    assert.equal(resultOf('workspace.restore'), 'Project')
    for (const m of [
      'session.rename',
      'session.archive',
      'session.unarchive',
      'session.restart',
      'session.promote',
    ]) {
      // 注册表里「无返回值」记作 null（线上是 `{}`），生成的类型是 `Empty`
      assert.equal(resultOf(m), null, m)
    }
    const text = readFileSync(METHOD_TYPES_TS, 'utf8')
    assert.match(
      text,
      /'workspace\.rename': \{ params: WorkspaceRenameParams; result: RenamedProject \}/,
    )
    for (const m of ['rename', 'archive', 'unarchive', 'restart', 'promote']) {
      assert.match(text, new RegExp(`'session\\.${m}': \\{ params: \\w+; result: Empty \\}`), m)
    }
    assert.match(text, /'workspace\.trash': \{ params: WorkspaceNameParams; result: TrashEntry \}/)
    assert.ok(!text.includes('ASSUMED'))
  })
})

describe('生成器', () => {
  const generatedSrc = 'export interface Empty {}\nexport interface A {}\nexport type B = string\n'

  it('类型名不存在时报错，而不是生成编译不过的文件', () => {
    assert.throws(
      () =>
        renderMethodTypes(
          [{ name: 'x.y', direction: 'request', params: 'Nope', result: null }],
          generatedSrc,
        ),
      /Nope/,
    )
  })

  it('方法名重复时报错', () => {
    const m: MethodSpec = { name: 'x.y', direction: 'request', params: null, result: null }
    assert.throws(() => renderMethodTypes([m, m], generatedSrc), /duplicate/)
  })

  it('null 变成 Empty；通知只有 params；长行折成多行', () => {
    const out = renderMethodTypes(
      [
        { name: 'x.y', direction: 'request', params: null, result: 'A' },
        { name: 'x.n', direction: 'notification', params: 'B', result: null },
      ],
      generatedSrc,
    )
    assert.match(out, /'x\.y': \{ params: Empty; result: A \}/)
    assert.match(out, /'x\.n': \{ params: B \}/)
    assert.match(out, /import type \{ A, B, Empty \} from '\.\/generated\.js'/)

    const long = 'L'.repeat(60)
    const out2 = renderMethodTypes(
      [{ name: 'x.long', direction: 'request', params: long, result: long }],
      `${generatedSrc}export interface ${long} {}\n`,
    )
    assert.match(out2, /'x\.long': \{\n {4}params: L+\n {4}result: L+\n {2}\}/)
  })
})
