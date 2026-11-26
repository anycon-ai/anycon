// 会话消息里的本地文件链接 → 文件浏览的路径、目录预览的目标（web.md § 4.6，ADR-62）。
import { describe, expect, it } from 'vitest'
import { absoluteToFsPath, classifyLocalHref, isWebPage, normalizeAbsolute, previewTargetOf, type LocalPathContext } from './localPaths'

const ctx: LocalPathContext = {
  workDir: 'hive',
  projects: [
    { name: 'hive', path: '/Users/me/anycon/hive' },
    { name: 'hive-docs', path: '/Users/me/anycon/hive-docs' },
    // 关联项目可以在别的项目里面：最长的匹配赢
    { name: 'inner', path: '/Users/me/anycon/hive/vendor/inner/' },
    { name: 'win', path: 'C:\\Work\\Win' },
  ],
  primaryRoot: '/Users/me/anycon',
}

describe('normalizeAbsolute', () => {
  it('collapses . and .., unifies separators, keeps the root', () => {
    expect(normalizeAbsolute('/a/./b/../c/')).toBe('/a/c')
    expect(normalizeAbsolute('/../..')).toBe('/')
    expect(normalizeAbsolute('c:\\Work\\..\\x')).toBe('C:/x')
    expect(normalizeAbsolute('relative/x')).toBeNull()
    expect(normalizeAbsolute('//server/share')).toBeNull()
    expect(normalizeAbsolute('/a\0b')).toBeNull()
  })
})

describe('absoluteToFsPath', () => {
  it('maps a path inside a project to <project>/<rest>, the longest project winning', () => {
    expect(absoluteToFsPath('/Users/me/anycon/hive/reports/a.html', ctx)).toBe('hive/reports/a.html')
    expect(absoluteToFsPath('/Users/me/anycon/hive', ctx)).toBe('hive')
    expect(absoluteToFsPath('/Users/me/anycon/hive-docs/x.md', ctx)).toBe('hive-docs/x.md')
    expect(absoluteToFsPath('/Users/me/anycon/hive/vendor/inner/r.html', ctx)).toBe('inner/r.html')
    expect(absoluteToFsPath('/Users/me/anycon/hive/.anycon/worktrees/x/r.html', ctx)).toBe('hive/.anycon/worktrees/x/r.html')
  })

  it('maps scratch working directories, but not .scratch itself', () => {
    expect(absoluteToFsPath('/Users/me/anycon/.scratch/sess_1/out/a.html', ctx)).toBe('.scratch/sess_1/out/a.html')
    expect(absoluteToFsPath('/Users/me/anycon/.scratch', ctx)).toBeNull()
  })

  it('does not match a sibling that only shares a prefix, or anything outside the projects', () => {
    expect(absoluteToFsPath('/Users/me/anycon/hivex/a', ctx)).toBeNull()
    expect(absoluteToFsPath('/Users/me/Downloads/report.html', ctx)).toBeNull()
    expect(absoluteToFsPath('/Users/me/anycon/notes.txt', ctx)).toBeNull()
    // `..` 走出项目目录
    expect(absoluteToFsPath('/Users/me/anycon/hive/../secret/a', ctx)).toBeNull()
  })

  it('compares Windows paths without regard to case', () => {
    expect(absoluteToFsPath('c:/work/win/Out/r.html', ctx)).toBe('win/Out/r.html')
    expect(absoluteToFsPath('C:\\WORK\\WIN\\a.txt', ctx)).toBe('win/a.txt')
  })
})

describe('classifyLocalHref', () => {
  it('leaves remote links alone', () => {
    for (const href of ['https://example.com/a.html', 'mailto:a@b.c', '//cdn.example/x', 'javascript:alert(1)']) {
      expect(classifyLocalHref(href, ctx)).toEqual({ kind: 'remote' })
    }
    expect(classifyLocalHref('#top', ctx)).toEqual({ kind: 'anchor' })
    expect(classifyLocalHref('', ctx)).toEqual({ kind: 'anchor' })
  })

  it('absolute paths, as the Markdown parser hands them over (percent-encoded)', () => {
    const href = `/Users/me/anycon/hive/reports/${encodeURIComponent('测试报告')}.html#summary`
    expect(classifyLocalHref(href, ctx)).toEqual({ kind: 'resolved', path: 'hive/reports/测试报告.html' })
    expect(classifyLocalHref('/Users/me/Downloads/qa/%E6%B5%8B.html', ctx)).toEqual({ kind: 'outside', display: '/Users/me/Downloads/qa/测.html' })
  })

  it('file: URLs and Windows drive paths', () => {
    expect(classifyLocalHref('file:///Users/me/anycon/hive/a.html', ctx)).toEqual({ kind: 'resolved', path: 'hive/a.html' })
    expect(classifyLocalHref('file://localhost/Users/me/anycon/hive/a%20b.html', ctx)).toEqual({ kind: 'resolved', path: 'hive/a b.html' })
    expect(classifyLocalHref('file:///C:/Work/Win/r.html', ctx)).toEqual({ kind: 'resolved', path: 'win/r.html' })
    expect(classifyLocalHref('file:///tmp/x.html', ctx)).toEqual({ kind: 'outside', display: '/tmp/x.html' })
    expect(classifyLocalHref('file://server/share/x.html', ctx).kind).toBe('outside')
    // micromark 把反斜杠编码成 %5C
    expect(classifyLocalHref('C:%5CWork%5CWin%5Cr.html', ctx)).toEqual({ kind: 'resolved', path: 'win/r.html' })
    expect(classifyLocalHref('C:/Users/me/r.html', ctx)).toEqual({ kind: 'outside', display: 'C:/Users/me/r.html' })
  })

  it('relative paths are relative to the session working directory and cannot leave the project', () => {
    expect(classifyLocalHref('reports/a.html', ctx)).toEqual({ kind: 'resolved', path: 'hive/reports/a.html' })
    expect(classifyLocalHref('./a/../b.png', ctx)).toEqual({ kind: 'resolved', path: 'hive/b.png' })
    expect(classifyLocalHref('reports%5Ca.html', ctx)).toEqual({ kind: 'resolved', path: 'hive/reports/a.html' })
    expect(classifyLocalHref('../other/a.html', ctx)).toEqual({ kind: 'outside', display: '../other/a.html' })
    const wt = { ...ctx, workDir: 'hive/.anycon/worktrees/x' }
    expect(classifyLocalHref('out/r.html', wt)).toEqual({ kind: 'resolved', path: 'hive/.anycon/worktrees/x/out/r.html' })
    // 走出 worktree 但还在项目里：可以
    expect(classifyLocalHref('../../../README.md', wt)).toEqual({ kind: 'resolved', path: 'hive/README.md' })
    expect(classifyLocalHref('a.html', { ...ctx, workDir: null })).toEqual({ kind: 'outside', display: 'a.html' })
    expect(classifyLocalHref('~/Downloads/a.html', ctx)).toEqual({ kind: 'outside', display: '~/Downloads/a.html' })
  })
})

describe('previewTargetOf / isWebPage', () => {
  it('splits the folder (first segment) from the URL path, encoding each segment', () => {
    expect(previewTargetOf('hive/reports/测试报告.html')).toEqual({ dir: 'hive', path: `/reports/${encodeURIComponent('测试报告.html')}` })
    expect(previewTargetOf('.scratch/sess_1/a b.html')).toEqual({ dir: '.scratch', path: '/sess_1/a%20b.html' })
    expect(previewTargetOf('hive/.anycon/worktrees/x/r.html')).toEqual({ dir: 'hive', path: '/.anycon/worktrees/x/r.html' })
    expect(previewTargetOf('hive/a#b?.html')).toEqual({ dir: 'hive', path: '/a%23b%3F.html' })
    expect(previewTargetOf('hive')).toEqual({ dir: 'hive', path: '/' })
    expect(previewTargetOf('')).toBeNull()
  })

  it('knows web pages by extension', () => {
    expect(isWebPage('a/b.html')).toBe(true)
    expect(isWebPage('a/B.HTM')).toBe(true)
    expect(isWebPage('a/b.html.md')).toBe(false)
    expect(isWebPage('a/b.xhtml')).toBe(false)
  })
})
