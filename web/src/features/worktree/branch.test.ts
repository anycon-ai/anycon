import { describe, expect, it } from 'vitest'
import { branchNameProblem, branchPlaceholder, isCommitBase, prBaseBranch } from './branch'

describe('branchNameProblem (basic git ref rules; the daemon has the last word)', () => {
  it('lets an empty name through: leaving it empty means “generate one”', () => {
    expect(branchNameProblem('')).toBeNull()
    expect(branchNameProblem('   ')).toBeNull()
  })

  it.each(['fix-login', 'anycon/fix-login', 'feature/a/b-c_d.e', 'v1.2.3', 'ünï/cødé', '用户/修复'])('accepts %s', (name) => {
    expect(branchNameProblem(name)).toBeNull()
  })

  it.each([
    'has space',
    'tab\tinside',
    'a..b',
    'a@{b',
    'a~1',
    'a^',
    'a:b',
    'a?',
    'a*',
    'a[0]',
    'a\\b',
    '-leading-dash',
    '/leading-slash',
    'trailing/',
    'trailing.',
    'double//slash',
    'seg/.hidden',
    '.hidden',
    'seg.lock',
    'dir/x.lock',
    '@',
    'a'.repeat(256),
  ])('rejects %j', (name) => {
    expect(branchNameProblem(name)).toBe('invalid')
  })

  it('rejects the names the daemon refuses for a new branch as reserved', () => {
    for (const name of ['main', 'master', 'HEAD', ' main ']) expect(branchNameProblem(name)).toBe('reserved')
    expect(branchNameProblem('main2')).toBeNull()
    expect(branchNameProblem('anycon/main')).toBeNull()
  })
})

describe('branch helpers', () => {
  it('builds the placeholder from the node’s branch prefix (an empty prefix means no prefix)', () => {
    expect(branchPlaceholder('anycon', 'auto')).toBe('anycon/auto')
    expect(branchPlaceholder('', 'auto')).toBe('auto')
    expect(branchPlaceholder(undefined, 'auto')).toBe('auto')
  })

  it('strips origin/ from the PR target but leaves other names alone', () => {
    expect(prBaseBranch('origin/main')).toBe('main')
    expect(prBaseBranch('main')).toBe('main')
    expect(prBaseBranch('release/1.0')).toBe('release/1.0')
    expect(prBaseBranch('upstream/main')).toBe('upstream/main')
  })
})

describe('isCommitBase (the daemon shows a commit base as the first 12 hex digits of base_sha)', () => {
  const sha = 'ce75846c4f1dd36dc6112c4e96765561ed711d93'
  it('is true for hex that is the start of base_sha', () => {
    expect(isCommitBase('ce75846c4f1d', sha)).toBe(true)
    expect(isCommitBase('CE75846C4F1D', sha)).toBe(true)
    expect(isCommitBase(sha, sha)).toBe(true)
  })
  it('is false for a branch, a tag, a remote-tracking branch', () => {
    for (const base of ['main', 'origin/main', 'v1.0', 'release/1.0']) expect(isCommitBase(base, sha)).toBe(false)
  })
  it('is false for a branch that merely looks like hex but is not the start of base_sha, or when base_sha is unknown', () => {
    expect(isCommitBase('dead', sha)).toBe(false)
    expect(isCommitBase('cafe1234', sha)).toBe(false)
    expect(isCommitBase('ce75846c4f1d', undefined)).toBe(false)
    expect(isCommitBase('ce75846c4f1d', null)).toBe(false)
    expect(isCommitBase('ce75846c4f1d', '')).toBe(false)
  })
})
