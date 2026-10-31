//! 会话：设置、摘要、事件流。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DeviceId, PermissionDetail, PermissionKind, PermissionOption, SessionId, TimestampMs};

/// 会话设置。`session.create` 和 `session.set_settings` 都收它；值来自 `AgentInfo`。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 所选模型的 `thinking.options[].id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// `AgentInfo.modes[].id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// `AgentInfo.options[]` 里的，按 id。
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub options: serde_json::Map<String, Value>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_append: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// 并发满，排队中。
    Queued,
    Starting,
    Running,
    WaitingPermission,
    Idle,
    Ended,
    Error,
    /// daemon 重启时发现的孤儿会话；下次 send 走 resume。
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub turns: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSummary {
    pub id: SessionId,
    /// 空 = scratch 会话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub agent: String,
    pub title: String,
    pub status: SessionStatus,
    #[serde(default)]
    pub archived: bool,
    pub settings: SessionSettings,
    pub created_at_ms: TimestampMs,
    pub updated_at_ms: TimestampMs,
    /// 最后一个事件的 seq；客户端据此决定要不要拉增量。
    pub last_seq: u64,
    pub pending_permissions: u32,
    /// agent 进程里还在跑的后台任务数（Claude Code 的后台 subagent、后台命令）。主 turn 结束后它们可能还在干活，
    /// 期间 daemon 不按 `idle_timeout` 回收进程（docs/spec/daemon.md § 7.3）。只反映 daemon 内存里的当前状态，不进事件流。
    #[serde(default)]
    pub background_tasks: u32,
    /// 排队位置（status = queued 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<u32>,
    #[serde(default)]
    pub usage: UsageTotals,
    /// 最后一条消息摘要（列表展示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// agent 自己的会话 id（Claude Code session_id 等），诊断用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// 会话在 anycon 建的 worktree 里工作时给出（ADR-34）；在项目目录本身工作时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<SessionWorktree>,
}

/// 会话在哪里工作。`session.create` 不带时是 `local`（线路上不猜默认值；客户端按节点的
/// `worktree.default` 和用户上次的选择决定要不要带 `worktree`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SessionWorkspace {
    /// 直接在项目目录里工作。
    Local,
    /// daemon 从项目新建一个 git worktree 和一条新分支，agent 在 worktree 里工作。
    /// 只有 `Project.git.worktree_ready` 的项目可以；scratch 会话不行。
    Worktree {
        /// 从哪个 ref 开分支：本地分支名、`origin/main` 这样的远端跟踪分支、标签，或提交 SHA。daemon 把分支 / 标签写法
        /// 规范成完整引用名记下（`refs/heads/main`、`refs/remotes/origin/main`、`refs/tags/v1.0`；展示时去掉前缀），其他能解析成提交的
        /// 写法（`HEAD`、`HEAD~1`…）只记解析出的提交，不再按名字重新解析。缺省 = `workspace.branches` 的 `default_base`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        /// 新分支的完整名字，必须是还不存在的分支。缺省 = `<worktree.branch_prefix>/<slug>`，
        /// slug 由 daemon 从第一条消息里的英文单词生成，取不到就随机两个词。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
}

/// 节点配置 `worktree.default` 的取值，也是 `SessionWorkspace` 的两种模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionWorkspaceMode {
    Local,
    Worktree,
}

/// 会话所在的 worktree。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionWorktree {
    /// `WorktreeInfo.id`。
    pub id: String,
    /// 如 `anycon/fix-login`。
    pub branch: String,
    /// base 的展示名：分支 / 远端跟踪分支 / 标签写法是短名字（`main`、`origin/main`、`v1.0`），
    /// 其他写法（`HEAD`、`HEAD~1`…）是解析出的提交的前 12 位。daemon 内部按规范化的引用名或完整 SHA 计算。
    pub base: String,
    /// 创建时 base 解析到的提交。
    pub base_sha: String,
    /// 相对项目目录的路径，如 `.anycon/worktrees/fix-login`。
    pub path: String,
    pub state: crate::WorktreeState,
}

/// 归档 / 删除会话时怎么处理它的 worktree。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeDisposal {
    /// 没有未提交的改动就删掉 worktree（分支上没有自己的提交时连分支一起删），有就保留。
    #[default]
    Auto,
    /// 保留 worktree。
    Keep,
    /// 删掉 worktree，丢弃未提交的改动。分支保留（除非 `delete_branch`）。
    Remove,
}

/// `session.archive`。没有 worktree 的会话只看 `session_id`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionArchiveParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub worktree: WorktreeDisposal,
}

/// `session.delete`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionDeleteParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub worktree: WorktreeDisposal,
    /// 同时删掉 worktree 的分支（`git branch -D`），分支上没合并的提交会丢。只有 worktree 也被删掉时才生效。
    #[serde(default)]
    pub delete_branch: bool,
}

/// `session.git_status`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionGitStatusParams {
    pub session_id: SessionId,
    /// 同时用 `gh` 查这条分支的 PR（要联网，约 1–3 秒）。没装 / 没登录 `gh`、没有远端时 `pr` 为空、`pr_error` 说明原因。
    #[serde(default)]
    pub include_pr: bool,
}

/// 会话工作目录的 git 状态。worktree 会话相对 base；在项目目录里工作的会话没有 base 相关字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionGitStatus {
    /// 当前分支；detached 时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub head_sha: String,
    /// worktree 会话的 base（同 `SessionWorktree.base`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// base 现在解析到的提交；base 已经不存在时退回创建时的提交。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// 本分支有、base 没有的提交数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    /// base 有、本分支还没有的提交数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
    /// 未提交的文件数（已暂存 + 未暂存 + 未跟踪）。
    pub uncommitted: u32,
    /// 有冲突、还没解决的文件数。
    pub conflicts: u32,
    /// 正在合并（`MERGE_HEAD` 存在）。
    #[serde(default)]
    pub merging: bool,
    /// 上游分支（`origin/anycon/fix-login`）；没推送过为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// 有上游时，本地还没推送的提交数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpushed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PullRequestInfo>,
    /// 要了 PR 但拿不到时的原因：`gh_not_installed` / `gh_not_authenticated` / `no_remote` / `no_pr` / `error`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Draft,
    Merged,
    Closed,
}

/// `gh pr view` 的摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PullRequestInfo {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub state: PullRequestState,
    /// PR 的目标分支。
    pub base_branch: String,
    /// CI 检查：通过 / 失败 / 进行中的数量。
    pub checks_passed: u32,
    pub checks_failed: u32,
    pub checks_pending: u32,
}

/// `session.diff` 比较的起点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiffAgainst {
    /// worktree 会话：从 base 和本分支的 merge-base 到工作区（已提交 + 未提交 + 未跟踪）。
    /// 在项目目录里工作的会话不支持（`invalid`）。
    #[default]
    Base,
    /// 从 HEAD 到工作区：未提交的改动（含未跟踪的文件）。
    Uncommitted,
}

/// `session.diff`：只读。不带 `path` 时只给文件列表和行数；带 `path` 时只给那一个文件，附 unified diff。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionDiffParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub against: DiffAgainst,
    /// 仓库内的相对路径（`DiffFile.path`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiffFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiffFile {
    /// 仓库内的相对路径（`/` 分隔）。
    pub path: String,
    /// 改名前的路径（`status = renamed`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: DiffFileStatus,
    /// 未跟踪的新文件（`status = added`）。
    #[serde(default)]
    pub untracked: bool,
    pub additions: u32,
    pub deletions: u32,
    #[serde(default)]
    pub binary: bool,
    /// 只在请求带 `path` 时给：这个文件的 unified diff（含 `diff --git` 头），最多 256 KiB。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// `patch` 被截断了。
    #[serde(default)]
    pub patch_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionDiffResult {
    /// 比较的起点提交（merge-base 或 HEAD）。
    pub from_sha: String,
    pub files: Vec<DiffFile>,
    /// 改动的文件超过 1000 个时只给前 1000 个。
    #[serde(default)]
    pub truncated: bool,
}

/// `session.sync_base`：把 base 的最新提交合并进 worktree 的分支（base 是远端跟踪分支时先 fetch）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSyncBaseParams {
    pub session_id: SessionId,
    /// 放弃一次有冲突的合并（`git merge --abort`）。
    #[serde(default)]
    pub abort: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcome {
    /// base 没有新提交。
    UpToDate,
    /// 合并完成（快进或合并提交）。
    Merged,
    /// 有冲突：合并停在中途，冲突文件里有标记。让 agent 解决，或者 `abort`。
    Conflicts,
    /// `abort` 成功。
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSyncBaseResult {
    pub outcome: SyncOutcome,
    /// 合进来的 base 提交数。
    pub commits: u32,
    /// 冲突的文件（最多 200 个）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionListResult {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionIdParams {
    pub session_id: SessionId,
}

/// 消息里的一个附件（ADR-35）：指向 daemon 上已经传完的 blob。客户端从 `blob.put_end` 拿到它、原样放进
/// `session.send` / `session.create`；daemon 以自己记录的元数据为准写进 `user_message` 事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttachmentRef {
    pub blob_id: String,
    pub name: String,
    pub mime: String,
    pub bytes: u64,
    /// 图片像素宽高（四种图片类型由 daemon 读文件头得出；读不出来、或者不是图片时省略）。客户端用来在下载前占好位置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionCreateParams {
    /// 空 = scratch。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub agent: String,
    /// 有则创建后立即发送第一条消息（`prompt` 为空但带了 `attachments` 时也发，内容为空串）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// 第一条消息的客户端 id（幂等），有 `prompt` 或 `attachments` 时必填。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default)]
    pub settings: SessionSettings,
    /// 第一条消息的附件（`blob.put_end` 的结果）。agent 不收其 mime（`capabilities.attachments` 既没点名也没有 `*/*`）→ `unsupported`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
    /// 在哪里工作；缺省 = `local`（ADR-34）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<SessionWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSendParams {
    pub session_id: SessionId,
    /// 客户端生成（uuid），重发不重复。
    pub message_id: String,
    /// 带了附件时可以为空串。
    pub content: String,
    /// `blob.put_end` 的结果，每条至多 20 个。agent 不收其 mime（`capabilities.attachments` 既没点名也没有 `*/*`）→ `unsupported`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSendResult {
    /// 立即开始的 turn id；进入队列（turn 进行中且 agent 不支持插话）时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub queued: bool,
    /// 幂等命中：这条 message_id 之前已收到。
    #[serde(default)]
    pub duplicate: bool,
}

/// `session.set_settings` 里可以被「清除」的字段。`SessionSettings` 的 `Option` 字段在线路上
/// 缺省和 `null` 等价、都表示「保持不变」，所以要清掉一个值得在 `clear` 里点名；`options` 不在
/// 这里：它的键值为 `null` 就是删键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClearableSetting {
    /// 回到 agent 默认模型（thinking 随之按新模型回落）。
    Model,
    Thinking,
    Mode,
    /// 清空整个 `env`。
    Env,
    SystemPromptAppend,
    MaxTurns,
    MaxBudgetUsd,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSetSettingsParams {
    pub session_id: SessionId,
    /// 增量：只带要改的字段；缺省 / `null` 表示保持（`options` 里值为 `null` 表示删键）。
    pub settings: SessionSettings,
    /// 要清除（恢复为未设置）的字段，在 `settings` 合并之后生效；两边都出现时以 `clear` 为准。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<ClearableSetting>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSetSettingsResult {
    /// 实际生效的完整设置（thinking 可能已按新模型回落）。
    pub settings: SessionSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionHistoryParams {
    pub session_id: SessionId,
    /// 返回 seq > after_seq 的事件。
    #[serde(default)]
    pub after_seq: u64,
    /// 默认 200，上限 1000。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// 为 true 时返回 seq < before_seq 的最近 limit 条（向上翻页）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionHistoryResult {
    pub events: Vec<SessionEvent>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionRenameParams {
    pub session_id: SessionId,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSearchParams {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchHit {
    pub session_id: SessionId,
    pub seq: u64,
    pub title: String,
    /// 带 `<mark>` 的片段。
    pub snippet: String,
    pub ts_ms: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSearchResult {
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionPromoteParams {
    pub session_id: SessionId,
    /// 新项目名。
    pub name: String,
}

/// `session.changed` 通知：列表级变化，发给所有在线客户端。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionChanged {
    pub session_id: SessionId,
    pub title: String,
    pub status: SessionStatus,
    #[serde(default)]
    pub archived: bool,
    pub pending_permissions: u32,
    /// 同 [`SessionSummary::background_tasks`]。
    #[serde(default)]
    pub background_tasks: u32,
    pub updated_at_ms: TimestampMs,
    pub last_seq: u64,
    /// 会话被删除。
    #[serde(default)]
    pub deleted: bool,
    /// worktree 的当前状态（会话有 worktree 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<SessionWorktree>,
}

// ---------- 事件 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    Shell,
    FileEdit,
    FileRead,
    Search,
    Web,
    Subagent,
    Mcp,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EditKind {
    Edit,
    Write,
    Delete,
    Move,
}

/// 按 category 归一化的渲染字段。全部可选；`input` 仍原样保留。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolCallDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_kind: Option<EditKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolResultDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified_diff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_count: Option<u32>,
    /// daemon 对 `output` 做头尾截断时丢掉的字节数（spec/daemon.md § 12）；没截断就缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_truncated_bytes: Option<u64>,
    /// 这次调用的流式 `tool_output_delta` 累计超过上限、后续 delta 没有转发；最终 `output` 仍是
    /// 头尾截断的完整输出。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_output_truncated: Option<bool>,
    /// 调用已经返回，但它启动的工作还在后台跑（后台子 agent）。结束时同一个 `id` 会再来一条
    /// `tool_result`（不带这个标记），取代这一条。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// 子 agent 结束时 agent 报的它一共用掉的 token（`category = subagent`；agent 不报就缺省）。
    /// 主 agent 的 `turn_done.usage` 不含这部分。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnDoneReason {
    Completed,
    Cancelled,
    Error,
    MaxTurns,
    Budget,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 模型的上下文窗口大小（token）。agent 不报就缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// turn 结束时上下文已经占用的 token：最近一次模型请求的输入 + 缓存读写。和 `input_tokens` 等
    /// 累计用量不同，它是「此刻窗口里有多少」，客户端拿它和 `context_window` 算占用比例。agent 不报就缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
}

/// 会话事件。adapter 把各 agent 的原生事件翻译成这个集合（docs/spec/daemon.md § 8）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// 用户消息也进事件流，历史回放才完整。
    UserMessage {
        message_id: String,
        content: String,
        by_device: DeviceId,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<AttachmentRef>,
    },
    /// `message_id` 是这个 turn 发出的第一条用户消息；**空串**表示 turn 是 agent 自己开始的，没有对应的用户消息
    /// （Claude Code 的后台任务结束后，CLI 会自己再跑一轮处理结果，见 docs/spec/daemon.md § 7.5）。
    TurnStart {
        turn_id: u64,
        message_id: String,
    },
    TextDelta {
        text: String,
    },
    TextDone {},
    /// `text` 可为空串（模型只给「正在思考」不给内容）。
    ThinkingDelta {
        text: String,
    },
    ThinkingDone {
        duration_ms: u64,
    },
    ToolCall {
        id: String,
        name: String,
        category: ToolCategory,
        title: String,
        #[serde(default)]
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ToolCallDetail>,
        /// 子 agent 内部的调用：启动这个子 agent 的那次调用（`category = subagent`）的 `id`。
        /// 子 agent 再启动子 agent 时一层层指上去；主 agent 自己的调用不带。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
    },
    ToolOutputDelta {
        id: String,
        text: String,
    },
    ToolResult {
        id: String,
        #[serde(default)]
        output: Value,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ToolResultDetail>,
    },
    FileChanged {
        path: String,
        kind: FileChangeKind,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    /// 同时也以 `permission.request` 通知发给所有在线客户端。
    Permission {
        request_id: String,
        kind: PermissionKind,
        title: String,
        detail: PermissionDetail,
        options: Vec<PermissionOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<TimestampMs>,
    },
    PermissionResolved {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option_id: Option<String>,
        by_device: DeviceId,
        /// 设备答复时带的 `answers`（kind = question：`{ [question_id]: 字符串或字符串数组 }`），原样记下，
        /// 客户端据此在历史里显示问题和回答。daemon 自己答复的（超时、取消、自动拒绝）没有。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answers: Option<serde_json::Map<String, Value>>,
        /// 设备答复时附的话（拒绝的理由、跳过问题时的说明）。daemon 自己答复的没有。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    TurnDone {
        turn_id: u64,
        reason: TurnDoneReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    Status {
        state: SessionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queued_messages: Option<u32>,
    },
    SettingsChanged {
        settings: SessionSettings,
    },
    TitleChanged {
        title: String,
    },
    /// daemon 自己做的 git 操作（不是 agent 做的）：worktree 建好 / 删掉 / 保留 / 重建 / 不见了，同步 base 的结果。
    /// 客户端渲染成一条居中的系统提示，并刷新 `session.git_status`。
    Git {
        action: GitAction,
        /// 会话 worktree 的分支。
        branch: String,
        /// `sync_*` 和 `worktree_created` 时给 base。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        /// `sync_merged`：合进来的提交数；`worktree_created` / `worktree_restored`：从 `.worktreeinclude` 复制的文件数。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
        /// `sync_conflicts`：冲突的文件（最多 50 个）。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
        /// `worktree_removed` 时是稳定取值：`branch_deleted`（分支一起删了，或者本来就不在了）/ `branch_kept`。
        /// 其他动作是补充说明（英文，调试用；比如 `worktree_failed` 的原因、`.worktreeinclude` 超过上限没复制完）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Error {
        code: String,
        message: String,
        recoverable: bool,
    },
    /// 后台任务（workflow、后台子 agent、后台命令）的状态快照。同一个 `id` 的后一条整条取代前一条，不是增量；
    /// 客户端按 `id` 只保留最新的一条。daemon 对同一个任务的快照节流（docs/spec/daemon.md § 8.3），
    /// 状态变化（进入终态，或者从终态回到 `running`）不受节流。
    Task {
        /// agent 给任务的 id（Claude Code 的 `task_id`；Codex 子 agent 的 thread id）。会话内唯一。
        id: String,
        kind: TaskKind,
        /// 启动这个任务的 `tool_call` 的 `id`（Claude Code 的 `tool_use_id`；Codex 的 collab agent 调用 item id）。
        /// 客户端据此把进度挂在那个工具块上；对不上时单独成块。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_id: Option<String>,
        /// workflow 的名字 / 任务描述。
        title: String,
        status: TaskStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TaskUsage>,
        /// 任务最近一次调用的工具名。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_tool: Option<String>,
        /// agent 给的进度 / 结果摘要。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// workflow 的阶段，按 `index` 升序；非 workflow 为空。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        phases: Vec<TaskPhase>,
        /// workflow 里的各个 agent，按 `index` 升序；非 workflow 为空。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        agents: Vec<TaskAgent>,
    },
    /// agent 给用户的提示：不是错误，不影响 turn，也不触发推送通知。客户端认识 `code` 时用自己的文案
    /// （`message` 作补充原文），不认识就按 `level` 显示 `message`。`code` 的取值见 docs/protocol.md § 6。
    Notice {
        code: String,
        level: NoticeLevel,
        /// agent 给的原文（通常是英文），可能是空串。`command_output` 里是本地命令的输出，可能多行。
        message: String,
        /// `code = model_fallback`：原来的模型。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_from: Option<String>,
        /// `code = model_fallback`：实际使用的模型。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_to: Option<String>,
        /// `code = rate_limit`：限额恢复的时间。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at_ms: Option<TimestampMs>,
    },
    /// turn 进行中 agent 在做的「非输出」的事（重试 API、压缩上下文）。客户端在 turn 进行中把它显示成一条临时状态；
    /// 收到任何内容事件（`text_delta`、`thinking_delta`、`tool_call`…）、`turn_done` 或 `state = none` 时清掉。
    /// 临时状态的快照语义同 `task`：后一条取代前一条。
    Activity {
        state: ActivityState,
        /// `retrying`：这是第几次重试（从 1 开始）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
        /// `retrying`：最多重试几次。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_attempts: Option<u32>,
        /// `retrying`：距下一次重试的毫秒数。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay_ms: Option<u64>,
        /// `retrying`：触发重试的错误摘要，如 `529 overloaded_error`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// `Event::Task` 的任务种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// 多阶段、多 agent 的 workflow：带 `phases` 和 `agents`。
    Workflow,
    /// 后台子 agent。
    Subagent,
    /// 后台命令。
    Shell,
    Other,
}

/// `Event::Task` 的任务状态。`Running` 以外的三个是收尾状态（终态）：Claude Code 的任务进入终态后不会再变；
/// Codex 的子 agent 是一个可以再收到输入的 thread，`Completed` 之后收到新输入、开始新 turn 就会回到 `Running`。
/// 所以终态只说明「这一刻不在跑」，不保证同一个 `id` 以后不会再有快照；客户端始终以最新的一条为准。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    /// 被取消，或者 agent 进程退出时任务还没收尾（daemon 补发，docs/spec/daemon.md § 8.3）。
    Cancelled,
}

impl TaskStatus {
    /// 终态：这一刻不在跑。不保证之后不会再变（Codex 的子 agent 可能从 `Completed` 回到 `Running`）。
    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskStatus::Running)
    }
}

/// 任务的用量。每个字段 agent 不报就缺省。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// workflow 的一个阶段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskPhase {
    pub index: u32,
    pub title: String,
}

/// workflow 里一个 agent 的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskAgentState {
    /// 还没轮到。
    Queued,
    Running,
    Done,
    Failed,
    /// 被跳过（上游失败、条件不满足…）。
    Skipped,
}

/// workflow 里的一个 agent。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskAgent {
    /// 在 workflow 里的序号，同一个任务内唯一。
    pub index: u32,
    pub label: String,
    pub state: TaskAgentState,
    /// 所属阶段的 `TaskPhase.index`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 最近一次调用的工具名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    /// `state = failed` 时的错误信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 第几次尝试（重试过才有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// 结果来自缓存，没有真的运行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached: Option<bool>,
}

/// `Event::Notice` 的级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
}

/// `Event::Activity` 的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    /// 临时状态结束。
    None,
    /// agent 正在重试一次失败的 API 请求。
    Retrying,
    /// agent 正在压缩上下文。
    Compacting,
}

/// `Event::Git` 的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitAction {
    /// worktree 建好了，agent 将在里面工作。
    WorktreeCreated,
    /// worktree 创建失败（随后有 `error` 事件）。
    WorktreeFailed,
    /// 归档 / 删除时 worktree 被删掉（`note` 说明分支是否一起删了）。
    WorktreeRemoved,
    /// 归档时因为有未提交的改动，worktree 被保留。
    WorktreeKept,
    /// 取消归档时按分支重建了 worktree。
    WorktreeRestored,
    /// worktree 目录不见了或被换掉，会话暂停。
    WorktreeMissing,
    SyncUpToDate,
    SyncMerged,
    SyncConflicts,
    SyncAborted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionEvent {
    pub session_id: SessionId,
    /// 会话内单调递增，持久化，用于增量拉取。
    pub seq: u64,
    pub ts_ms: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub event: Event,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_tagging() {
        let e = Event::TextDelta { text: "hi".into() };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "text_delta");
        let s = SessionSettings::default();
        assert_eq!(serde_json::to_string(&s).unwrap(), "{}");
    }

    #[test]
    fn worktree_wire_shapes() {
        // 工作位置按 `mode` 打标签；不带 workspace 的旧请求照常解析。
        let w = SessionWorkspace::Worktree { base: Some("origin/main".into()), branch: None };
        assert_eq!(serde_json::to_value(&w).unwrap(), serde_json::json!({"mode": "worktree", "base": "origin/main"}));
        assert_eq!(serde_json::to_value(SessionWorkspace::Local).unwrap(), serde_json::json!({"mode": "local"}));
        let p: SessionCreateParams = serde_json::from_value(serde_json::json!({"agent": "mock"})).unwrap();
        assert_eq!(p.workspace, None);

        // archive / delete 换了参数类型，只带 session_id 的旧请求仍然合法，处置方式缺省 auto。
        let a: SessionArchiveParams = serde_json::from_value(serde_json::json!({"session_id": "s"})).unwrap();
        assert_eq!(a.worktree, WorktreeDisposal::Auto);
        let d: SessionDeleteParams = serde_json::from_value(serde_json::json!({"session_id": "s"})).unwrap();
        assert_eq!((d.worktree, d.delete_branch), (WorktreeDisposal::Auto, false));

        let e = Event::Git {
            action: GitAction::SyncConflicts,
            branch: "anycon/x".into(),
            base: Some("main".into()),
            count: None,
            files: vec!["a.rs".into()],
            note: None,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "git");
        assert_eq!(v["action"], "sync_conflicts");
    }

    fn roundtrip(e: &Event) -> serde_json::Value {
        let v = serde_json::to_value(e).unwrap();
        let back: Event = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(&back, e);
        v
    }

    #[test]
    fn task_event_wire_shape() {
        let e = Event::Task {
            id: "t1".into(),
            kind: TaskKind::Workflow,
            tool_id: Some("toolu_1".into()),
            title: "review".into(),
            status: TaskStatus::Running,
            usage: Some(TaskUsage { total_tokens: Some(1200), tool_calls: Some(3), duration_ms: None }),
            last_tool: Some("Read".into()),
            summary: None,
            phases: vec![TaskPhase { index: 0, title: "scan".into() }],
            agents: vec![TaskAgent {
                index: 0,
                label: "scanner".into(),
                state: TaskAgentState::Queued,
                phase_index: Some(0),
                model: None,
                tokens: None,
                tool_calls: None,
                duration_ms: None,
                last_tool: None,
                error: None,
                attempt: None,
                cached: Some(true),
            }],
        };
        let v = roundtrip(&e);
        assert_eq!(v["type"], "task");
        assert_eq!(v["kind"], "workflow");
        assert_eq!(v["status"], "running");
        assert_eq!(v["agents"][0]["state"], "queued");
        assert_eq!(v["usage"], serde_json::json!({"total_tokens": 1200, "tool_calls": 3}));
        assert!(v.get("summary").is_none());

        // 最小形态：只有必填字段，其余缺省；phases / agents 为空时不上线路。
        let min = Event::Task {
            id: "t2".into(),
            kind: TaskKind::Subagent,
            tool_id: None,
            title: "t".into(),
            status: TaskStatus::Cancelled,
            usage: None,
            last_tool: None,
            summary: None,
            phases: vec![],
            agents: vec![],
        };
        let v = roundtrip(&min);
        assert_eq!(
            v,
            serde_json::json!({"type": "task", "id": "t2", "kind": "subagent", "title": "t", "status": "cancelled"})
        );

        // 所有枚举取值都是 snake_case。
        for (k, s) in [(TaskKind::Shell, "shell"), (TaskKind::Other, "other")] {
            assert_eq!(serde_json::to_value(k).unwrap(), s);
        }
        for (st, s) in [
            (TaskAgentState::Running, "running"),
            (TaskAgentState::Done, "done"),
            (TaskAgentState::Failed, "failed"),
            (TaskAgentState::Skipped, "skipped"),
        ] {
            assert_eq!(serde_json::to_value(st).unwrap(), s);
        }
        assert!(!TaskStatus::Running.is_terminal());
        for st in [TaskStatus::Completed, TaskStatus::Failed, TaskStatus::Cancelled] {
            assert!(st.is_terminal());
        }
    }

    #[test]
    fn notice_event_wire_shape() {
        let e = Event::Notice {
            code: "model_fallback".into(),
            level: NoticeLevel::Warning,
            message: "switched".into(),
            model_from: Some("opus".into()),
            model_to: Some("sonnet".into()),
            resets_at_ms: None,
        };
        let v = roundtrip(&e);
        assert_eq!(v["type"], "notice");
        assert_eq!(v["level"], "warning");
        assert_eq!(v["model_from"], "opus");
        assert!(v.get("resets_at_ms").is_none());

        let e = Event::Notice {
            code: "rate_limit".into(),
            level: NoticeLevel::Info,
            message: String::new(),
            model_from: None,
            model_to: None,
            resets_at_ms: Some(1_700_000_000_000),
        };
        let v = roundtrip(&e);
        assert_eq!(v["level"], "info");
        assert_eq!(v["resets_at_ms"], 1_700_000_000_000u64);
        // message 是必填字段，空串照常上线路。
        assert_eq!(v["message"], "");
    }

    #[test]
    fn activity_event_wire_shape() {
        let e = Event::Activity {
            state: ActivityState::Retrying,
            attempt: Some(2),
            max_attempts: Some(10),
            delay_ms: Some(5000),
            reason: Some("529 overloaded_error".into()),
        };
        let v = roundtrip(&e);
        assert_eq!(v["type"], "activity");
        assert_eq!(v["state"], "retrying");
        assert_eq!(v["delay_ms"], 5000);

        let none = Event::Activity {
            state: ActivityState::None,
            attempt: None,
            max_attempts: None,
            delay_ms: None,
            reason: None,
        };
        assert_eq!(roundtrip(&none), serde_json::json!({"type": "activity", "state": "none"}));
        let compacting = serde_json::json!({"type": "activity", "state": "compacting"});
        let e: Event = serde_json::from_value(compacting).unwrap();
        assert!(matches!(e, Event::Activity { state: ActivityState::Compacting, attempt: None, .. }));
    }

    #[test]
    fn usage_context_fields_are_optional_on_the_wire() {
        // 旧数据（没有上下文字段）照常解析；没有值时不上线路。
        let old: Usage = serde_json::from_value(serde_json::json!({"duration_ms": 5, "input_tokens": 1})).unwrap();
        assert_eq!((old.context_window, old.context_tokens), (None, None));
        assert!(serde_json::to_value(&old).unwrap().get("context_window").is_none());

        let u = Usage { context_window: Some(200_000), context_tokens: Some(48_000), ..Default::default() };
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!((v["context_window"].as_u64(), v["context_tokens"].as_u64()), (Some(200_000), Some(48_000)));
        let e = Event::TurnDone { turn_id: 1, reason: TurnDoneReason::Completed, usage: Some(u) };
        roundtrip(&e);
    }
}
