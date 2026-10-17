// 各方法的返回类型来自 schema/methods.json 生成的 method-types，SDK 的封装不再有手写的 Empty / 错类型。
// 这里的断言是编译期的（`bun run typecheck` 会检查）；运行期只确认文件被加载。
import assert from 'node:assert/strict'
import { it } from 'node:test'

import type {
  AttachmentRef,
  BlobGetResult,
  BlobPutBeginParams,
  BrowseEntry,
  Empty,
  LinkBlock,
  NotificationMethods,
  PairAck,
  Project,
  RenamedProject,
  RequestMethods,
  SessionArchiveParams,
  SessionDeleteParams,
  SessionDiffParams,
  SessionDiffResult,
  SessionGitStatus,
  SessionGitStatusParams,
  SessionSyncBaseParams,
  SessionSyncBaseResult,
  SessionWorkspace,
  SessionWorktree,
  TrashEntry,
  WorkspaceBranchesParams,
  WorkspaceBranchesResult,
  WorkspaceBrowseParams,
  WorkspaceBrowseResult,
  WorkspaceLinkParams,
  WorkspaceNameParams,
  WorkspaceRemoveWorktreeParams,
  WorkspaceWorktreesParams,
  WorkspaceWorktreesResult,
  WorktreeDisposal,
  WorktreeInfo,
} from '@anycon/protocol'

import type {
  AttachmentError,
  AttachmentErrorReason,
  AttachmentRef as SdkAttachmentRef,
  BrowseEntry as SdkBrowseEntry,
  CreateSessionParams,
  DownloadedAttachment,
  SendOptions,
  LinkBlock as SdkLinkBlock,
  Node,
  Session,
  SessionWorktree as SdkSessionWorktree,
  WorktreeDisposal as SdkWorktreeDisposal,
} from '../src/index.js'

type Equal<A, B> =
  (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false
type Expect<T extends true> = T
type Resolved<F> = F extends (...args: never[]) => Promise<infer R> ? R : never

// SDK 封装的返回类型（workspace.rename 答 `{ from, to }`；session.rename / archive / … 答 `{}`，句柄方法是 void）
export type SdkTypes = [
  Expect<Equal<Resolved<Node['projects']['rename']>, RenamedProject>>,
  Expect<Equal<Resolved<Node['projects']['trash']>, TrashEntry>>,
  Expect<Equal<Resolved<Node['projects']['restore']>, Project>>,
  Expect<Equal<Resolved<Node['projects']['create']>, Project>>,
  // 关联已有目录：link → Project，unlink → Empty（`{}`），browse → WorkspaceBrowseResult
  Expect<Equal<Resolved<Node['projects']['link']>, Project>>,
  Expect<Equal<Resolved<Node['projects']['unlink']>, Empty>>,
  Expect<Equal<Resolved<Node['projects']['browse']>, WorkspaceBrowseResult>>,
  Expect<Equal<Resolved<Session['rename']>, void>>,
  Expect<Equal<Resolved<Session['archive']>, void>>,
  Expect<Equal<Resolved<Session['unarchive']>, void>>,
  Expect<Equal<Resolved<Session['restart']>, void>>,
  Expect<Equal<Resolved<Session['delete']>, void>>,
  // worktree 与 git（ADR-34）：返回类型直接是协议类型，不再包一层
  Expect<Equal<Resolved<Node['projects']['branches']>, WorkspaceBranchesResult>>,
  Expect<Equal<Resolved<Node['worktrees']['list']>, WorktreeInfo[]>>,
  Expect<Equal<Resolved<Node['worktrees']['remove']>, Empty>>,
  Expect<Equal<Resolved<Session['gitStatus']>, SessionGitStatus>>,
  Expect<Equal<Resolved<Session['diff']>, SessionDiffResult>>,
  Expect<Equal<Resolved<Session['syncBase']>, SessionSyncBaseResult>>,
  Expect<Equal<Session['worktree'], SessionWorktree | undefined>>,
  Expect<Equal<SdkSessionWorktree, SessionWorktree>>,
  Expect<Equal<SdkWorktreeDisposal, WorktreeDisposal>>,
]

// 新方法的参数类型：驼峰选项 → 位置参数 / 选项对象
export type SdkParams = [
  Expect<Equal<Parameters<Node['projects']['link']>, [path: string, options?: { name?: string }]>>,
  Expect<Equal<Parameters<Node['projects']['unlink']>, [name: string]>>,
  Expect<
    Equal<
      Parameters<Node['projects']['browse']>,
      [options?: { path?: string; showHidden?: boolean }]
    >
  >,
  // worktree 与 git：驼峰选项 → 线路上的 snake_case（见 worktree.test.ts）
  Expect<Equal<Parameters<Node['projects']['branches']>, [project: string]>>,
  Expect<
    Equal<
      Parameters<Node['worktrees']['remove']>,
      [id: string, options?: { force?: boolean; deleteBranch?: boolean }]
    >
  >,
  Expect<Equal<Parameters<Node['worktrees']['list']>, [options?: { project?: string }]>>,
  Expect<Equal<Parameters<Session['gitStatus']>, [options?: { includePr?: boolean }]>>,
  Expect<Equal<Parameters<Session['syncBase']>, [options?: { abort?: boolean }]>>,
  Expect<Equal<Parameters<Session['archive']>, [options?: { worktree?: WorktreeDisposal }]>>,
  Expect<
    Equal<
      Parameters<Session['delete']>,
      [options?: { worktree?: WorktreeDisposal; deleteBranch?: boolean }]
    >
  >,
  Expect<Equal<CreateSessionParams['workspace'], SessionWorkspace | undefined>>,
  Expect<Equal<CreateSessionParams['prompt'], string | undefined>>,
]

// 关联目录的「不能关联的原因」：LinkBlock 是五个字面量的联合；`blocked` 在条目和浏览结果上都是可选的
// （已经是项目时为空，看 `project`），线上也可能是 null。SDK 从 index 重新导出 LinkBlock。
export type BlockedFields = [
  Expect<Equal<LinkBlock, 'hidden' | 'sensitive' | 'workspace' | 'data_dir' | 'filesystem_root'>>,
  Expect<Equal<SdkLinkBlock, LinkBlock>>,
  Expect<Equal<BrowseEntry['blocked'], LinkBlock | null | undefined>>,
  Expect<Equal<WorkspaceBrowseResult['blocked'], LinkBlock | null | undefined>>,
  Expect<
    Equal<
      Resolved<Node['projects']['browse']>['entries'][number]['blocked'],
      LinkBlock | null | undefined
    >
  >,
  // SDK 从 index 重新导出的 BrowseEntry 与协议包的是同一个类型
  Expect<Equal<SdkBrowseEntry, BrowseEntry>>,
]

// 图片附件（ADR-35）：上传返回 daemon 的记录，原样放进 send / create；拒绝时 reason 是收窄的联合
export type Attachments = [
  Expect<Equal<Resolved<Node['uploadImage']>, AttachmentRef>>,
  Expect<Equal<Resolved<Node['downloadAttachment']>, DownloadedAttachment>>,
  Expect<Equal<DownloadedAttachment['ref'], AttachmentRef>>,
  Expect<Equal<Resolved<Node['discardAttachment']>, void>>,
  Expect<Equal<Parameters<Node['discardAttachment']>, [attachment: AttachmentRef | string]>>,
  Expect<Equal<CreateSessionParams['attachments'], AttachmentRef[] | undefined>>,
  Expect<Equal<SendOptions['attachments'], AttachmentRef[] | undefined>>,
  Expect<Equal<SdkAttachmentRef, AttachmentRef>>,
  Expect<Equal<AttachmentError['reason'], AttachmentErrorReason>>,
  Expect<Equal<RequestMethods['blob.put_begin']['params'], BlobPutBeginParams>>,
  Expect<Equal<RequestMethods['blob.put_end']['result'], AttachmentRef>>,
  Expect<Equal<RequestMethods['blob.get']['result'], BlobGetResult>>,
]

// 注册表本身
export type Registry = [
  Expect<Equal<RequestMethods['workspace.rename']['result'], RenamedProject>>,
  Expect<Equal<RequestMethods['workspace.trash']['result'], TrashEntry>>,
  Expect<Equal<RequestMethods['workspace.restore']['result'], Project>>,
  Expect<Equal<RequestMethods['workspace.link']['params'], WorkspaceLinkParams>>,
  Expect<Equal<RequestMethods['workspace.link']['result'], Project>>,
  Expect<Equal<RequestMethods['workspace.unlink']['params'], WorkspaceNameParams>>,
  Expect<Equal<RequestMethods['workspace.unlink']['result'], Empty>>,
  Expect<Equal<RequestMethods['workspace.browse']['params'], WorkspaceBrowseParams>>,
  Expect<Equal<RequestMethods['workspace.browse']['result'], WorkspaceBrowseResult>>,
  Expect<Equal<RequestMethods['workspace.branches']['params'], WorkspaceBranchesParams>>,
  Expect<Equal<RequestMethods['workspace.branches']['result'], WorkspaceBranchesResult>>,
  Expect<Equal<RequestMethods['workspace.worktrees']['params'], WorkspaceWorktreesParams>>,
  Expect<Equal<RequestMethods['workspace.worktrees']['result'], WorkspaceWorktreesResult>>,
  Expect<
    Equal<RequestMethods['workspace.remove_worktree']['params'], WorkspaceRemoveWorktreeParams>
  >,
  Expect<Equal<RequestMethods['workspace.remove_worktree']['result'], Empty>>,
  Expect<Equal<RequestMethods['session.archive']['params'], SessionArchiveParams>>,
  Expect<Equal<RequestMethods['session.delete']['params'], SessionDeleteParams>>,
  Expect<Equal<RequestMethods['session.git_status']['params'], SessionGitStatusParams>>,
  Expect<Equal<RequestMethods['session.git_status']['result'], SessionGitStatus>>,
  Expect<Equal<RequestMethods['session.diff']['params'], SessionDiffParams>>,
  Expect<Equal<RequestMethods['session.diff']['result'], SessionDiffResult>>,
  Expect<Equal<RequestMethods['session.sync_base']['params'], SessionSyncBaseParams>>,
  Expect<Equal<RequestMethods['session.sync_base']['result'], SessionSyncBaseResult>>,
  Expect<Equal<RequestMethods['session.rename']['result'], Empty>>,
  Expect<Equal<RequestMethods['session.archive']['result'], Empty>>,
  Expect<Equal<RequestMethods['session.unarchive']['result'], Empty>>,
  Expect<Equal<RequestMethods['session.restart']['result'], Empty>>,
  Expect<Equal<RequestMethods['session.promote']['result'], Empty>>,
  Expect<Equal<NotificationMethods['pair.ack']['params'], PairAck>>,
]

it('method-types 的类型断言在编译期检查（见 typecheck）', () => {
  assert.ok(true)
})
