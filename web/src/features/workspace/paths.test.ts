import { describe, expect, it } from 'vitest'
import { baseName, caseInsensitiveHost, crumbsFor, defaultProjectName, nameClash, nfcName, projectNameProblem } from './paths'

describe('baseName / defaultProjectName', () => {
  it('takes the last segment and ignores trailing separators', () => {
    expect(baseName('/Users/me/code/app')).toBe('app')
    expect(baseName('/Users/me/code/app/')).toBe('app')
    expect(baseName('C:\\work\\app')).toBe('app')
    expect(baseName('/')).toBe('')
    expect(baseName('C:\\')).toBe('')
  })

  it('strips leading dots, since a project name cannot start with one', () => {
    expect(defaultProjectName('/home/me/.dotfiles')).toBe('dotfiles')
    expect(defaultProjectName('/home/me/app')).toBe('app')
    expect(defaultProjectName('/')).toBe('')
  })
})

describe('projectNameProblem', () => {
  it('follows the daemon rules', () => {
    expect(projectNameProblem('my-app')).toBeNull()
    expect(projectNameProblem('  my app  ')).toBeNull()
    expect(projectNameProblem('')).toBe('empty')
    expect(projectNameProblem('   ')).toBe('empty')
    expect(projectNameProblem('a/b')).toBe('invalid')
    expect(projectNameProblem('.hidden')).toBe('invalid')
    expect(projectNameProblem('x'.repeat(256))).toBe('too_long')
    // 255 字节，不是 255 个字符
    expect(projectNameProblem('目'.repeat(86))).toBe('too_long')
    expect(projectNameProblem('目'.repeat(85))).toBeNull()
  })
})

describe('projectNameProblem: backslash and control characters', () => {
  it.each([
    ['a\\b', 'a backslash'],
    ['a\u0000b', 'NUL'],
    ['a\u0007b', 'BEL'],
    ['a\nb', 'a newline inside the name'],
    ['a\u001bb', 'ESC'],
    ['a\u007fb', 'DEL'],
    ['a\u0085b', 'a C1 control character'],
  ])('rejects %j (%s)', (name) => {
    expect(projectNameProblem(name)).toBe('invalid')
  })

  it('still accepts ordinary and non-ASCII names, and ignores surrounding whitespace', () => {
    expect(projectNameProblem('my-app_2')).toBeNull()
    expect(projectNameProblem('项目 α')).toBeNull()
    expect(projectNameProblem('  spaced\n')).toBeNull() // 首尾空白会被去掉，发给 daemon 的也是去掉之后的
  })
})

describe('nameClash (NFC, and case-insensitive where the filesystem is)', () => {
  it('treats the NFC and NFD spellings of a name as the same name', () => {
    const nfc = 'Caf\u00e9'
    const nfd = 'Cafe\u0301'
    expect(nfd).not.toBe(nfc)
    expect(nfcName(nfd)).toBe(nfc)
    expect(nameClash([nfc], nfd)).toEqual({ kind: 'exact', with: nfc })
    expect(nameClash([nfd], nfc)).toEqual({ kind: 'exact', with: nfd })
  })

  it('reports a difference in case only as a case clash, so the caller can decide whether the filesystem counts it', () => {
    expect(nameClash(['proj'], 'PROJ')).toEqual({ kind: 'case', with: 'proj' })
    expect(nameClash(['Proj', 'other'], 'proj')).toEqual({ kind: 'case', with: 'Proj' })
    // 同时有完全相同的就是 exact
    expect(nameClash(['PROJ', 'proj'], 'proj')).toEqual({ kind: 'exact', with: 'proj' })
    expect(nameClash(['Caf\u00e9'], 'CAFE\u0301')).toEqual({ kind: 'case', with: 'Caf\u00e9' })
  })

  it('finds nothing for different names, and ignores surrounding whitespace', () => {
    expect(nameClash(['app', 'api'], 'apps')).toBeNull()
    expect(nameClash([], 'x')).toBeNull()
    expect(nameClash(['app'], '  app ')).toEqual({ kind: 'exact', with: 'app' })
  })

  it('knows which nodes default to a case-insensitive filesystem', () => {
    expect(caseInsensitiveHost('macos-aarch64')).toBe(true)
    expect(caseInsensitiveHost('darwin-x86_64')).toBe(true)
    expect(caseInsensitiveHost('windows-x86_64')).toBe(true)
    expect(caseInsensitiveHost('linux-x86_64')).toBe(false)
    expect(caseInsensitiveHost(undefined)).toBe(false)
  })
})

describe('crumbsFor', () => {
  it('starts at the root that contains the path and never goes above it', () => {
    expect(crumbsFor('/home/me/code/app', ['/home/me'])).toEqual([
      { label: 'me', path: '/home/me' },
      { label: 'code', path: '/home/me/code' },
      { label: 'app', path: '/home/me/code/app' },
    ])
  })

  it('has a single crumb at a root', () => {
    expect(crumbsFor('/home/me', ['/home/me'])).toEqual([{ label: 'me', path: '/home/me' }])
    expect(crumbsFor('/home/me/', ['/home/me'])).toEqual([{ label: 'me', path: '/home/me' }])
  })

  it('picks the nearest root when roots nest, and does not confuse siblings with a shared prefix', () => {
    const roots = ['/home/me', '/home/me/work']
    expect(crumbsFor('/home/me/work/app', roots).map((c) => c.path)).toEqual(['/home/me/work', '/home/me/work/app'])
    expect(crumbsFor('/home/me2/app', ['/home/me'])).toEqual([{ label: '/home/me2/app', path: '/home/me2/app' }])
  })

  it('handles the filesystem root and Windows paths', () => {
    expect(crumbsFor('/srv/app', ['/'])).toEqual([
      { label: '/', path: '/' },
      { label: 'srv', path: '/srv' },
      { label: 'app', path: '/srv/app' },
    ])
    expect(crumbsFor('C:\\work\\app', ['C:\\work'])).toEqual([
      { label: 'work', path: 'C:\\work' },
      { label: 'app', path: 'C:\\work\\app' },
    ])
  })
})
