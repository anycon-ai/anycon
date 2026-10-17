// 「改动」标签里的快捷消息（ADR-34）：让 agent 提交、创建 PR、解决冲突。文案按界面语言，带上分支和 base；
// 经 `session.send` 发给 agent——推送和 `gh` 走 agent 的权限审批，提交信息和 PR 描述由 agent 写（daemon 不调模型）。
import type { TFunction } from 'i18next'
import { isCommitBase, prBaseBranch } from './branch'

export function commitMessage(t: TFunction, branch: string): string {
  return t('git.messages.commit', { branch })
}

/** base 是提交而不是分支时不写目标分支，让 agent 用仓库的默认分支——往一个 SHA 开 PR 没有意义。 */
export function pullRequestMessage(t: TFunction, branch: string, base: string, baseSha?: string | null): string {
  return isCommitBase(base, baseSha) ? t('git.messages.prDefault', { branch }) : t('git.messages.pr', { branch, base: prBaseBranch(base) })
}

/**
 * 不列文件名：冲突文件的名字来自 base 分支的内容（可能是别人的提交），拼进**用户**角色的消息就是把仓库内容抬成了用户指令。
 * 让 agent 自己看 `git status`——它本来就会读到同样的名字，只是信任度不同。
 */
export function resolveConflictsMessage(t: TFunction, branch: string, base: string): string {
  return t('git.messages.resolve', { branch, base })
}
