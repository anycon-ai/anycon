use serde::{Deserialize, Serialize};

use crate::TimestampMs;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub dirty: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// 能给这个项目的会话开 worktree：项目目录就是仓库顶层（不是仓库里的子目录），并且至少有一个提交
    /// （ADR-34）。为 false 时客户端不提供「新 worktree」。
    #[serde(default, skip_serializing_if = "is_false")]
    pub worktree_ready: bool,
}

/// 项目从哪来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    /// workspace 根目录下的一级子文件夹（文件夹即项目，不需要注册）。
    #[default]
    Folder,
    /// 用 `workspace.link` 关联的本机已有目录（在根目录之外）。解除关联不动目录里的任何文件。
    Linked,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 一个项目 = workspace 根目录下的一个一级子文件夹，或一个关联的本机目录（`kind = linked`）。
/// 元数据全部推导；只有「关联了哪些目录」存在 daemon 的库里。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Project {
    /// 项目名。根目录下的项目和关联的项目共用一个命名空间，客户端的所有操作都用它。
    pub name: String,
    /// 绝对路径，仅展示用。
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitMeta>,
    pub modified_at_ms: TimestampMs,
    pub active_sessions: u32,
    /// 缺省 `folder`。
    #[serde(default)]
    pub kind: ProjectKind,
    /// 关联的目录已经不在了（被移走、删除，或外接盘没挂上）。只有 `linked` 会是 true；
    /// 这时 `git` 为空、`modified_at_ms` 为 0，不能在里面新建会话，客户端应提供「解除关联」。
    #[serde(default, skip_serializing_if = "is_false")]
    pub missing: bool,
    /// git 元数据还在算（daemon 冷缓存时前台等待超时）：这时 `git` 为空，**不代表不是 git 项目**。
    /// 算完之后 daemon 发 `workspace.changed { updated }`，客户端重新 `workspace.list`。
    #[serde(default, skip_serializing_if = "is_false")]
    pub git_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceListResult {
    pub root: String,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceCreateParams {
    pub name: String,
    #[serde(default)]
    pub git_init: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceCloneParams {
    /// 只允许 `https://`、`ssh://`、`git@host:path` 三种形式。
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `workspace.link`：把本机一个已有目录关联为项目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceLinkParams {
    /// 本机绝对路径；`~` / `~/…` 按 daemon 用户的 HOME 展开。必须是已存在的目录，
    /// canonicalize 之后要在 `workspace.link_roots` 之内。
    pub path: String,
    /// 项目名；缺省取目录名。规则同 `workspace.create`，不能和已有项目（根目录下的或关联的）重名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `workspace.browse`：给「关联已有目录」的选择器列本机目录。只列目录，只在 `workspace.link_roots` 之内。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceBrowseParams {
    /// 要列的目录（绝对路径或 `~/…`）。缺省 = `roots` 的第一项。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 也列出以 `.` 开头的目录。默认 false。
    #[serde(default)]
    pub show_hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceBrowseResult {
    /// canonicalize 之后的绝对路径。
    pub path: String,
    /// 上一级的绝对路径；`path` 已经是某个 root 时为空（不能再往上）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// 可浏览、可关联的范围：`workspace.link_roots` 展开并 canonicalize 后的绝对路径。
    pub roots: Vec<String>,
    /// `path` 本身能不能直接关联（规则同 `BrowseEntry.linkable`）。
    pub linkable: bool,
    /// `path` 本身已经是项目时给出项目名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// `path` 本身不能关联、又不是因为已经是项目时，给出原因（规则同 `BrowseEntry.blocked`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<LinkBlock>,
    /// 子目录，按名字排序。跟随符号链接；解析后落在 `roots` 之外的不列。
    pub entries: Vec<BrowseEntry>,
    /// 子目录超过 1000 个时只给前 1000 个。
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BrowseEntry {
    pub name: String,
    /// 绝对路径，可以直接传给 `workspace.browse` / `workspace.link`。
    pub path: String,
    /// 目录下有 `.git`。
    #[serde(default, skip_serializing_if = "is_false")]
    pub git: bool,
    /// 已经是项目（根目录下的或已关联的）时给出项目名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 能不能关联：不是项目，并且 `blocked` 为空。
    pub linkable: bool,
    /// 不能关联的原因（已经是项目时为空，看 `project`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<LinkBlock>,
}

/// 一个目录为什么不能关联为项目（ADR-33 修订，daemon.md § 10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinkBlock {
    /// 从它所在的 link root 往下，路径里有以 `.` 开头的一段（`~/.x`、`~/work/.y`）。
    /// 运维把那个目录（或它的上级隐藏目录）显式写进 `workspace.link_roots` 才放行。
    Hidden,
    /// 内置拒绝表里的系统 / 凭证 / 自启动目录（`~/Library`、`~/.ssh`、`/etc`…），在它们里面，或包含它们。
    /// 只有 `link_roots` 的某一项本身就在那个目录里面时，那一项之下才放行。
    Sensitive,
    /// 在 workspace 根目录里（它已经是项目，或在 `.trash` / `.scratch` 里），或包含 workspace 根目录。
    Workspace,
    /// 是 anycon 数据目录（`ANYCON_HOME`）、在它里面，或包含它。不能放行。
    DataDir,
    /// 文件系统根。
    FilesystemRoot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobStarted {
    pub job_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceRenameParams {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceNameParams {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TrashEntry {
    /// `.trash/` 下的目录名，如 `my-app-20261011T120000Z`。
    pub id: String,
    pub original_name: String,
    pub trashed_at_ms: TimestampMs,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceTrashListResult {
    pub entries: Vec<TrashEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceTrashIdParams {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RenamedProject {
    pub from: String,
    pub to: String,
}

/// `workspace.changed` 通知。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceChanged {
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default)]
    pub renamed: Vec<RenamedProject>,
    /// 这些项目推导出来的元数据（`git`）有了或变了——比如 daemon 重启后冷缓存里第一次算完（ADR-34）。
    /// 客户端重新 `workspace.list`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updated: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobProgress {
    pub job_id: String,
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_line: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobDone {
    pub job_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobCancelParams {
    pub job_id: String,
}

// ---- fs（只读）----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FsListParams {
    /// `<项目名>/<项目内的相对路径>`，如 `my-app/src`。第一段是关联项目的名字时，其余部分相对那个目录解析，
    /// 并且不能走出它；否则相对 workspace 根解析（`""` 列根目录本身，不含关联项目）。
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
    pub bytes: u64,
    pub modified_at_ms: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FsListResult {
    pub entries: Vec<FsEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FsReadParams {
    /// 解析规则同 `FsListParams.path`。
    pub path: String,
    /// 默认 1 MiB，上限 10 MiB。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FsReadResult {
    /// UTF-8 文本；二进制文件 `text` 为空且 `binary = true`。
    pub text: String,
    pub binary: bool,
    pub truncated: bool,
    pub bytes: u64,
}

// ---- 分支与 worktree（ADR-34）----

/// `workspace.branches`：给新建会话的「基于哪个分支」下拉框列分支。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceBranchesParams {
    pub project: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceBranchesResult {
    /// 项目目录当前检出的分支；HEAD 是 detached 时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// 新会话默认的 base：当前分支有上游（`origin/main`）时是上游，否则是当前分支；detached 时是 HEAD 的提交。
    pub default_base: String,
    /// 本地分支，按最近提交时间倒序，最多 200 个。不含 anycon 自己建的 worktree 分支（`<branch_prefix>/…`）。
    pub local: Vec<String>,
    /// 远端跟踪分支（`origin/main`），同样排序和上限，不含 `*/HEAD`。
    pub remote: Vec<String>,
    /// 任何一组被截断时为 true。
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// anycon 建的 worktree 现在的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeState {
    /// 会话刚创建，daemon 正在 `git worktree add` / 复制 `.worktreeinclude` 列出的文件。
    Creating,
    /// 目录在，会话可以在里面工作。
    Active,
    /// 会话已归档，但 worktree 里有未提交的改动，所以留在了磁盘上。
    Kept,
    /// worktree 目录已经删掉（分支还在，除非它没有自己的提交）。取消归档时按分支重建。
    Removed,
    /// 目录不见了，或者被换成了别的目录 / 符号链接。会话不能继续，直到按分支重建。
    Missing,
    /// 创建失败（会话同时进入 `error`）。磁盘上没有留下半成品。
    Failed,
}

/// 一个由 anycon 建的 worktree。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorktreeInfo {
    /// daemon 生成的 id（`wt_` + 22 位 base62）。
    pub id: String,
    pub project: String,
    /// 相对项目目录的路径，如 `.anycon/worktrees/fix-login`。
    pub path: String,
    pub branch: String,
    /// base 的展示名，规则同 `SessionWorktree.base`（短引用名，或提交的前 12 位）。
    pub base: String,
    /// 所属会话；会话删掉之后为空（「没有会话的 worktree」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    pub state: WorktreeState,
    /// 未提交的文件数（含未跟踪的）；目录不在时为 0。
    pub uncommitted: u32,
    /// 分支上 base 之后自己的提交数。
    pub ahead: u32,
    /// 磁盘占用；统计超过时间预算时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub created_at_ms: TimestampMs,
}

/// `workspace.worktrees`。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceWorktreesParams {
    /// 只列这个项目的；缺省 = 全部项目。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceWorktreesResult {
    pub worktrees: Vec<WorktreeInfo>,
}

/// `workspace.remove_worktree`：删掉一个已保留或没有会话的 worktree。会话还没归档时返回 `busy`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceRemoveWorktreeParams {
    pub id: String,
    /// 有未提交的改动也删（丢弃这些改动）。为 false 而有改动时返回 `invalid`，`data.reason = "uncommitted_changes"`。
    #[serde(default)]
    pub force: bool,
    /// 同时删掉分支（`git branch -D`）。分支上有没合并的提交也会丢。
    #[serde(default)]
    pub delete_branch: bool,
}
