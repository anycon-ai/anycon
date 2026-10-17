import { afterEach, describe, expect, it } from 'vitest'
import i18n from '@/i18n'
import type { GitAction } from '@/protocol'
import type { GitSystemBlock } from '@/features/session/reducer'
import { gitEventText } from './eventText'

const block = (over: Partial<GitSystemBlock> & { action: GitAction }): GitSystemBlock => ({ key: 'k', seq: 1, ts: 1, type: 'system', kind: 'git', branch: 'anycon/x', files: [], ...over })
const en = i18n.getFixedT('en')
const zh = i18n.getFixedT('zh')

afterEach(() => i18n.changeLanguage('en'))

describe('gitEventText', () => {
  it('says the same things in English and Chinese for every action', () => {
    const cases: Array<[GitSystemBlock, string, string]> = [
      [block({ action: 'worktree_created', base: 'main' }), 'Started in worktree anycon/x (based on main)', '已在 worktree anycon/x 中开始（基于 main）'],
      [block({ action: 'worktree_created', base: 'main', count: 1 }), 'Started in worktree anycon/x (based on main, copied 1 file)', '已在 worktree anycon/x 中开始（基于 main，复制了 1 个文件）'],
      [block({ action: 'worktree_created', base: 'main', count: 0 }), 'Started in worktree anycon/x (based on main)', '已在 worktree anycon/x 中开始（基于 main）'],
      [block({ action: 'worktree_failed', note: 'boom' }), 'Could not create worktree anycon/x: boom', 'worktree anycon/x 创建失败：boom'],
      [block({ action: 'worktree_failed' }), 'Could not create worktree anycon/x', 'worktree anycon/x 创建失败'],
      [block({ action: 'worktree_removed' }), 'Deleted worktree anycon/x', '已删除 worktree anycon/x'],
      [block({ action: 'worktree_removed', note: 'branch_kept' }), 'Deleted worktree anycon/x (branch kept)', '已删除 worktree anycon/x（分支保留）'],
      [block({ action: 'worktree_removed', note: 'branch_deleted' }), 'Deleted worktree anycon/x (branch deleted)', '已删除 worktree anycon/x（分支已删除）'],
      [block({ action: 'worktree_removed', note: 'something unexpected' }), 'Deleted worktree anycon/x', '已删除 worktree anycon/x'],
      [block({ action: 'worktree_kept' }), 'Worktree anycon/x has uncommitted changes, so it was kept', 'worktree anycon/x 有未提交的改动，已保留'],
      [block({ action: 'worktree_restored' }), 'Rebuilt worktree anycon/x from its branch', '已按分支重建 worktree anycon/x'],
      [block({ action: 'worktree_missing' }), 'The folder of worktree anycon/x is gone', 'worktree anycon/x 的目录不见了'],
      [block({ action: 'sync_up_to_date', base: 'main' }), 'main has no new commits', 'main 没有新提交'],
      [block({ action: 'sync_merged', base: 'main', count: 3 }), 'Merged 3 commits from main', '已合并 main 的 3 个提交'],
      [block({ action: 'sync_merged', base: 'main', count: 1 }), 'Merged 1 commit from main', '已合并 main 的 1 个提交'],
      [block({ action: 'sync_conflicts', base: 'main', files: ['a.rs', 'b.rs'] }), 'Merging main: 2 files have conflicts (a.rs, b.rs)', '合并 main 时 2 个文件有冲突（a.rs、b.rs）'],
      [block({ action: 'sync_aborted', base: 'main' }), 'Merge of main abandoned', '已放弃合并 main'],
    ]
    for (const [b, e, z] of cases) {
      expect(gitEventText(en, b)).toBe(e)
      expect(gitEventText(zh, b)).toBe(z)
    }
  })

  it('lists at most four conflicting files and counts the rest', () => {
    const files = ['a', 'b', 'c', 'd', 'e', 'f']
    expect(gitEventText(en, block({ action: 'sync_conflicts', base: 'main', files }))).toBe('Merging main: 6 files have conflicts (a, b, c, d and 2 more)')
    expect(gitEventText(zh, block({ action: 'sync_conflicts', base: 'main', files }))).toBe('合并 main 时 6 个文件有冲突（a、b、c、d 等 2 个）')
  })

  it('does not print “undefined” when the daemon leaves the base out', () => {
    expect(gitEventText(en, block({ action: 'sync_up_to_date' }))).toBe('the base has no new commits')
  })
})
