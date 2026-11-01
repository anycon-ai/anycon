import { describe, expect, it } from 'vitest'
import type { FsEntry } from '@/protocol'
import { baseName, chain, crumbsOf, fileKind, isHidden, joinPath, parentOf, resolveLocalUrl, sessionWorkDir, sortEntries, splitPath, treeRoot } from './paths'

describe('path helpers', () => {
  it('splits and joins without empty or `.` segments, and leaves `..` for the daemon to judge', () => {
    expect(splitPath('a//b/./c/')).toEqual(['a', 'b', 'c'])
    expect(splitPath('')).toEqual([])
    expect(splitPath('a/../b')).toEqual(['a', '..', 'b'])
    expect(joinPath('a/b', 'c')).toBe('a/b/c')
    expect(joinPath('', 'x')).toBe('x')
    expect(parentOf('a/b/c.ts')).toBe('a/b')
    expect(parentOf('a')).toBe('')
    expect(baseName('a/b/c.ts')).toBe('c.ts')
    expect(baseName('')).toBe('')
  })

  it('tree root is the project, or `.scratch/<id>` for a scratch session', () => {
    expect(treeRoot('')).toBe('')
    expect(treeRoot('proj')).toBe('proj')
    expect(treeRoot('proj/src/a.ts')).toBe('proj')
    expect(treeRoot('proj/.anycon/worktrees/fix/src')).toBe('proj')
    expect(treeRoot('.scratch/sess_1')).toBe('.scratch/sess_1')
    expect(treeRoot('.scratch/sess_1/notes.md')).toBe('.scratch/sess_1')
    expect(treeRoot('.scratch')).toBe('.scratch')
  })

  it('chain lists every level from the root down to the path', () => {
    expect(chain('p', 'p/a/b.ts')).toEqual(['p', 'p/a', 'p/a/b.ts'])
    expect(chain('p', 'p')).toEqual(['p'])
    expect(chain('.scratch/s', '.scratch/s/x/y')).toEqual(['.scratch/s', '.scratch/s/x', '.scratch/s/x/y'])
    expect(chain('p', 'q/a')).toEqual(['q/a'])
    expect(chain('', '')).toEqual([])
  })

  it('crumbs carry the full path of every level', () => {
    expect(crumbsOf('p/a/b.ts')).toEqual([
      { label: 'p', path: 'p' },
      { label: 'a', path: 'p/a' },
      { label: 'b.ts', path: 'p/a/b.ts' },
    ])
  })

  it('hidden means a leading dot', () => {
    expect(isHidden('.git')).toBe(true)
    expect(isHidden('a.git')).toBe(false)
  })
})

describe('sortEntries', () => {
  const e = (name: string, is_dir: boolean): FsEntry => ({ name, is_dir, bytes: 0, modified_at_ms: 0 })
  it('puts folders first, then sorts by name ignoring case and comparing numbers by value', () => {
    const sorted = sortEntries([e('b.txt', false), e('Zeta', true), e('a10.txt', false), e('a2.txt', false), e('alpha', true), e('B.md', false)])
    expect(sorted.map((x) => x.name)).toEqual(['alpha', 'Zeta', 'a2.txt', 'a10.txt', 'B.md', 'b.txt'])
  })
  it('does not mutate its input', () => {
    const input = [e('b', false), e('a', false)]
    sortEntries(input)
    expect(input.map((x) => x.name)).toEqual(['b', 'a'])
  })
})

describe('resolveLocalUrl', () => {
  const at = (url: string, dir = 'p/docs', root = 'p'): string | null => resolveLocalUrl(url, dir, root)
  it('resolves relative paths against the folder of the file', () => {
    expect(at('img/a.png')).toBe('p/docs/img/a.png')
    expect(at('./a.md')).toBe('p/docs/a.md')
    expect(at('../README.md')).toBe('p/README.md')
    expect(at('sub/../a.md')).toBe('p/docs/a.md')
    expect(at('guide/')).toBe('p/docs/guide')
  })
  it('treats a leading slash as relative to the project root, like the usual Markdown renderers', () => {
    expect(at('/src/main.rs')).toBe('p/src/main.rs')
    expect(at('/')).toBe('p')
  })
  it('drops the query and hash and decodes percent escapes', () => {
    expect(at('a%20b.png?raw=1#x')).toBe('p/docs/a b.png')
    expect(at('guide.md#install')).toBe('p/docs/guide.md')
  })
  it('refuses to leave the root', () => {
    expect(at('../../x')).toBeNull()
    expect(at('../..')).toBeNull()
    expect(at('/../x')).toBeNull()
    expect(resolveLocalUrl('../x', '.scratch/s/d', '.scratch/s')).toBe('.scratch/s/x')
    expect(resolveLocalUrl('../../x', '.scratch/s/d', '.scratch/s')).toBeNull()
  })
  it('returns null for anything that is not a local path', () => {
    for (const u of ['', '#top', 'https://example.com/a.png', 'http://x/y', '//evil.example/a.png', 'mailto:a@b.c', 'data:image/png;base64,AAAA', 'javascript:alert(1)', 'blob:x', 'a%00b', 'a\\b', '%E0%A4%A'])
      expect(at(u), u).toBeNull()
  })
})

describe('fileKind', () => {
  it('recognises images by extension, case-insensitively', () => {
    expect(fileKind('a.PNG')).toEqual({ type: 'image', mime: 'image/png' })
    expect(fileKind('a.jpg')).toEqual({ type: 'image', mime: 'image/jpeg' })
    expect(fileKind('a.jpeg')).toEqual({ type: 'image', mime: 'image/jpeg' })
    expect(fileKind('a.gif')).toEqual({ type: 'image', mime: 'image/gif' })
    expect(fileKind('a.webp')).toEqual({ type: 'image', mime: 'image/webp' })
    expect(fileKind('logo.svg')).toEqual({ type: 'image', mime: 'image/svg+xml' })
  })
  it('recognises Markdown', () => {
    expect(fileKind('README.md')).toEqual({ type: 'markdown' })
    expect(fileKind('notes.markdown')).toEqual({ type: 'markdown' })
  })
  it('picks the highlight language from the extension or the file name', () => {
    expect(fileKind('main.rs')).toEqual({ type: 'text', lang: 'rust' })
    expect(fileKind('app.tsx')).toEqual({ type: 'text', lang: 'tsx' })
    expect(fileKind('Dockerfile')).toEqual({ type: 'text', lang: 'dockerfile' })
    expect(fileKind('Cargo.lock')).toEqual({ type: 'text', lang: 'toml' })
    expect(fileKind('a.test.ts')).toEqual({ type: 'text', lang: 'typescript' })
  })
  it('falls back to plain text, and a dotfile name is not an extension', () => {
    expect(fileKind('notes.xyz')).toEqual({ type: 'text' })
    expect(fileKind('LICENSE')).toEqual({ type: 'text' })
    expect(fileKind('.png')).toEqual({ type: 'text' })
    expect(fileKind('.gitignore')).toEqual({ type: 'text' })
  })
})

describe('sessionWorkDir', () => {
  const wt = (state: 'active' | 'kept' | 'creating' | 'missing' | 'removed' | 'failed') => ({ path: '.anycon/worktrees/fix', state })
  it('is the worktree folder of a worktree session', () => {
    expect(sessionWorkDir({ id: 's1', project: 'app', worktree: wt('active') }, {})).toBe('app/.anycon/worktrees/fix')
    expect(sessionWorkDir({ id: 's1', project: 'app', worktree: wt('kept') }, {})).toBe('app/.anycon/worktrees/fix')
  })
  it('falls back to the project folder while the worktree does not exist', () => {
    for (const s of ['creating', 'missing', 'removed', 'failed'] as const) expect(sessionWorkDir({ id: 's1', project: 'app', worktree: wt(s) }, {})).toBe('app')
  })
  it('is the project for a session working in the project folder', () => {
    expect(sessionWorkDir({ id: 's1', project: 'app', worktree: null }, {})).toBe('app')
  })
  it('is `.scratch/<id>` for a scratch session', () => {
    expect(sessionWorkDir({ id: 'sess_abc', project: null }, undefined)).toBe('.scratch/sess_abc')
  })
  it('is null when the project is gone or its folder is missing', () => {
    expect(sessionWorkDir({ id: 's1', project: 'app' }, undefined)).toBeNull()
    expect(sessionWorkDir({ id: 's1', project: 'app' }, { missing: true })).toBeNull()
  })
})
