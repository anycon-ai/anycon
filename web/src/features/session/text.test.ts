import { parseDiff } from 'react-diff-view'
import { describe, expect, it } from 'vitest'
import type { ToolBlock } from './reducer'
import { diffStats, normalizeDiff, outputToText, syntheticDiff, toolCommand, toolFileChanges, toolLabel, toolOutputText } from './text'

const tool = (over: Partial<ToolBlock> = {}): ToolBlock => ({ key: 'k', seq: 1, ts: 1, type: 'tool', toolId: 't', name: 'Bash', category: 'shell', title: 'echo hi', input: {}, streamed: '', ...over })

describe('tool text helpers', () => {
  it('flattens text content arrays and objects', () => {
    expect(outputToText([{ type: 'text', text: 'a' }, { type: 'text', text: 'b' }])).toBe('a\nb')
    expect(outputToText({ text: 'x' })).toBe('x')
    expect(outputToText({ a: 1 })).toBe('{\n  "a": 1\n}')
    expect(outputToText(null)).toBe('')
  })
  it('prefers the final result output over streamed deltas', () => {
    expect(toolOutputText(tool({ streamed: 'partial' }))).toBe('partial')
    expect(toolOutputText(tool({ streamed: 'partial', result: { output: 'full', isError: false } }))).toBe('full')
    expect(toolOutputText(tool({ streamed: 'streamed', result: { output: '', isError: false } }))).toBe('streamed')
  })
  it('takes the command from detail, then input, then title', () => {
    expect(toolCommand(tool({ detail: { command: 'ls' } }))).toBe('ls')
    expect(toolCommand(tool({ input: { command: 'pwd' } }))).toBe('pwd')
    expect(toolCommand(tool())).toBe('echo hi')
  })
  it('builds a diff from edit input and counts changes', () => {
    const d = syntheticDiff(tool({ category: 'file_edit', input: { file_path: 'a.ts', old_string: 'x', new_string: 'y\nz' } }))!
    expect(diffStats(d)).toEqual({ added: 2, removed: 1 })
    expect(syntheticDiff(tool({ input: {} }))).toBeNull()
  })
})

// Codex adapter 给的 `tool_call` / `tool_result` 形状（crates/adapters/src/codex/tools.rs）：name 是 item 类型，不是 Bash / Edit。
describe('tool labels and diffs for Codex-shaped tool calls (keyed on title / detail, never on the tool name)', () => {
  it('toolLabel prefers the title, then detail server/tool, then detail.description, and only then the raw name', () => {
    expect(toolLabel(tool({ name: 'mcp__github__search_issues', category: 'mcp', title: 'github/search_issues', detail: { server: 'github', tool: 'search_issues' } }))).toBe('github/search_issues')
    expect(toolLabel(tool({ name: 'mcp__github__search_issues', category: 'mcp', title: '', detail: { server: 'github', tool: 'search_issues' } }))).toBe('github/search_issues')
    expect(toolLabel(tool({ name: 'spawnAgent', category: 'subagent', title: '', detail: { description: 'Explore the repo' } }))).toBe('Explore the repo')
    expect(toolLabel(tool({ name: 'imageGeneration', category: 'other', title: '' }))).toBe('imageGeneration')
  })

  const changes = [
    { path: '/work/app/new.txt', kind: 'add', diff: 'hello\nworld\n' },
    { path: '/work/app/src/lib.rs', kind: 'update', diff: '@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn c() {}\n' },
    { path: '/work/app/old.txt', kind: 'delete', diff: 'bye\n' },
    { path: '/work/app/m.txt', kind: 'update', move_path: '/work/app/n.txt', diff: '@@ -1 +1 @@\n-x\n+y\n\n\nMoved to: /work/app/n.txt' },
  ]
  const fileChange = tool({ name: 'fileChange', category: 'file_edit', title: 'Edit 4 files', input: { changes }, detail: { path: '/work/app/new.txt', edit_kind: 'write' } })

  it('reads input.changes of a multi-file fileChange', () => {
    const got = toolFileChanges(fileChange)
    expect(got.map((c) => [c.path, c.kind, c.movePath])).toEqual([
      ['/work/app/new.txt', 'add', undefined],
      ['/work/app/src/lib.rs', 'update', undefined],
      ['/work/app/old.txt', 'delete', undefined],
      ['/work/app/m.txt', 'update', '/work/app/n.txt'],
    ])
    expect(toolFileChanges(tool({ input: { changes: 'nope' } }))).toEqual([])
    expect(toolFileChanges(tool({ input: { changes: [1, { path: '' }, { path: 'a' }] } })).map((c) => c.path)).toEqual(['a'])
  })

  it('builds one diff per file from input.changes (new, edited, deleted, moved) so an approval can show it before the patch is applied', () => {
    const diff = syntheticDiff(fileChange)!
    expect(diffStats(diff)).toEqual({ added: 2 + 1 + 1, removed: 1 + 1 + 1 })
    const files = parseDiff(diff, { nearbySequences: 'zip' })
    expect(files.map((f) => [f.type, f.oldPath, f.newPath])).toEqual([
      ['add', '/dev/null', 'work/app/new.txt'],
      ['modify', 'work/app/src/lib.rs', 'work/app/src/lib.rs'],
      ['delete', 'work/app/old.txt', '/dev/null'],
      ['modify', 'work/app/m.txt', 'work/app/n.txt'],
    ])
    expect(diff).not.toContain('Moved to')
  })

  it('still builds Claude-style edit diffs from old_string / new_string', () => {
    expect(syntheticDiff(tool({ category: 'file_edit', input: { file_path: 'a.ts', old_string: 'x', new_string: 'y' } }))).toContain('+++ b/a.ts')
  })
})

describe('normalizeDiff', () => {
  // adapter 的 `unified_diff`：几个文件首尾相连，只有 `---` / `+++`，没有 `diff --git`
  const adapter = ['--- /dev/null', '+++ b/a.txt', '@@ -0,0 +1,2 @@', '+hi there', '+second line', '--- a/b.txt', '+++ b/c.txt', '@@ -1 +1 @@', '-x', '+y', '--- a/d.txt', '+++ /dev/null', '@@ -1,1 +0,0 @@', '-bye', ''].join('\n')

  it('gives every file its own `diff --git` header, so a multi-file Codex diff is not rendered as one file', () => {
    expect(parseDiff(adapter, { nearbySequences: 'zip' })).toHaveLength(1) // 为什么要规整：不规整时解析器把它们合成了一个
    const files = parseDiff(normalizeDiff(adapter), { nearbySequences: 'zip' })
    expect(files.map((f) => [f.type, f.oldPath, f.newPath, f.hunks.length])).toEqual([
      ['add', '/dev/null', 'a.txt', 1],
      ['modify', 'b.txt', 'c.txt', 1],
      ['delete', 'd.txt', '/dev/null', 1],
    ])
  })

  it('leaves a diff that already has `diff --git` headers alone', () => {
    const d = 'diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b'
    expect(normalizeDiff(d)).toBe(d)
  })

  it('does not take a removed line that starts with "-- " for a file header', () => {
    const d = ['--- a/a.sql', '+++ b/a.sql', '@@ -1,3 +1,2 @@', ' select 1;', '--- comment', '+++ not a header either', ' select 2;', ''].join('\n')
    // 这里 hunk 头说旧 3 行 / 新 2 行：`--- comment`（删）和 `+++ …`（增）都在 hunk 里
    expect(normalizeDiff(d).match(/^diff --git /gm)).toHaveLength(1)
  })
})
