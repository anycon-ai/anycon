use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DeviceId, DeviceKind, TimestampMs};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeCapabilities {
    /// 有 workspace（文件夹即项目）。SDK 节点可能没有。
    pub workspace: bool,
    pub fs: bool,
    pub search: bool,
    pub blobs: bool,
    pub audit: bool,
    /// 支持每会话 worktree、`session.git_status` / `diff` / `sync_base`（ADR-34）。daemon 找不到 `git` 时为 false。
    #[serde(default)]
    pub worktree: bool,
    /// 支持 dev server 预览（`preview.*`，ADR-39），且节点配置 `preview.enabled = true`。
    #[serde(default)]
    pub preview: bool,
}

/// 节点的 worktree 配置（`config.toml` 的 `[worktree]`），客户端据此决定新建会话的初始选择和分支名预览。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeWorktreeSettings {
    /// git 项目里新会话默认在哪里工作。
    pub default_mode: crate::SessionWorkspaceMode,
    /// 新分支名的前缀（`anycon` → `anycon/<slug>`）；空串 = 不加前缀。
    pub branch_prefix: String,
}

/// `node.info` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeInfo {
    pub node_id: DeviceId,
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    pub capabilities: NodeCapabilities,
    pub started_at_ms: TimestampMs,
    /// `capabilities.worktree` 为 true 时给出。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<NodeWorktreeSettings>,
}

/// 已配对的客户端设备（daemon 视角）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PeerDevice {
    pub device_id: DeviceId,
    pub name: String,
    pub kind: DeviceKind,
    /// Noise 静态公钥的指纹（短 hex）。
    pub fingerprint: String,
    pub paired_at_ms: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<TimestampMs>,
    pub online: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeDevicesResult {
    pub devices: Vec<PeerDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeRevokeDeviceParams {
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodePairBeginParams {
    /// `true`（缺省）= 16 位短码（约 79 bit；链接、二维码、默认都是它）；`false` = 8 位（约 40 bit），
    /// 只适合在自建 hub 上手输（ADR-29 修订：拿到 match_id 的恶意 hub 可以离线穷举 8 位短码）。
    #[serde(default = "default_true")]
    pub long: bool,
}

impl Default for NodePairBeginParams {
    fn default() -> Self {
        Self { long: true }
    }
}

fn default_true() -> bool {
    true
}

/// `node.pair_begin` 结果。短码由 daemon 本地生成，hub 只拿到 match id。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodePairBeginResult {
    /// `XXXX-XXXX` 或 `XXXX-XXXX-XXXX-XXXX`。
    pub code: String,
    pub expires_at_ms: TimestampMs,
    /// `${hub_public_url}/pair#code=XXXX-XXXX`。短码放在 URL fragment 里，浏览器不会把它发给 hub。
    pub link: String,
}

/// `pair.ack`：握手完成且 `PairDone` 之后，daemon 经新建立的 Noise 传输发给 joiner 的第一条通知。
/// joiner 校验 `peer_id == 自己的正式 id`、`node_id == PairDone.peer` 后才落盘节点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PairAck {
    /// daemon 的正式 device id。
    pub node_id: DeviceId,
    /// hub 告诉 daemon 的 joiner 正式 id。
    pub peer_id: DeviceId,
    pub name: String,
    pub version: String,
    /// daemon 静态公钥指纹（joiner 可与握手得到的对比）。
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionCounts {
    pub queued: u32,
    pub running: u32,
    pub waiting_permission: u32,
    pub idle: u32,
    pub ended: u32,
    pub error: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeStats {
    pub uptime_ms: u64,
    pub hub_connected: bool,
    pub peers_online: u32,
    pub sessions: SessionCounts,
    pub agent_processes: u32,
    pub db_bytes: u64,
    pub disk_free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DoctorCheck {
    pub id: String,
    pub ok: bool,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeDoctorResult {
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeAuditParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AuditEntry {
    pub ts_ms: TimestampMs,
    pub device_id: DeviceId,
    pub device_name: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeAuditResult {
    pub entries: Vec<AuditEntry>,
}

/// `node.set_agent_defaults`：写 `config.agents.<id>.default_*`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeSetAgentDefaultsParams {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<serde_json::Map<String, Value>>,
}

/// 一项配置能不能被远程客户端改（ADR-43）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEditable {
    /// 任意改（在取值约束之内）。
    Any,
    /// 只能往更严的方向改：开关只能从 `true` 改成 `false`。想放宽要在节点机器上 `anycon config set`。
    TightenOnly,
    /// 只读：不在白名单里的键不会出现在列表里；出现在这里的 `no` 是被 `ANYCON_*` 环境变量覆盖的键
    /// （环境变量盖过文件，远程改了也不生效）。
    No,
}

/// 配置值的类型，客户端据此选控件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigValueKind {
    /// JSON 布尔。
    Bool,
    /// JSON 整数，范围见 [`ConfigItem::min`] / [`ConfigItem::max`]。
    Int,
    /// 时长字符串（`30s`、`20m`、`2h`、`7d`，纯数字按秒；`0` 在有「不限」含义的键上表示不限），范围（毫秒）见 `min` / `max`。
    Duration,
    /// 字符串，取值是 [`ConfigItem::options`] 之一。
    Enum,
    /// 自由文本，长度（字符数）上限见 `max`；内容另有键自己的规则，不合法时 `node.set_config` 回 `invalid`。
    Text,
}

/// `node.get_config` 的一项：一个可以远程查看的配置键。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConfigItem {
    /// 点分隔的键，和 `config.toml` 一致（`sessions.max_concurrent`）。
    pub key: String,
    /// 当前生效的值（默认值 → 文件 → 环境变量合并后的结果）：布尔、整数或字符串，类型见 `kind`。
    pub value: Value,
    pub kind: ConfigValueKind,
    pub editable: ConfigEditable,
    /// 被 `ANYCON_*` 环境变量覆盖（`log.level` 还包括 `ANYCON_LOG` / `RUST_LOG`）。为真时 `editable` 一定是 `no`。
    #[serde(default)]
    pub env_override: bool,
    /// `kind = enum` 时允许的取值。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// 取值下限：`int` 是数值，`duration` 是毫秒，`text` 不用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u64>,
    /// 取值上限：`int` 是数值，`duration` 是毫秒，`text` 是字符数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
}

/// `node.get_config` / `node.set_config` 的结果：白名单里的全部配置项（顺序固定，按段分组）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeConfigResult {
    pub items: Vec<ConfigItem>,
}

/// `node.set_config` 的一项改动。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConfigChange {
    pub key: String,
    /// 新值，JSON 类型要和该键的 `kind` 一致（布尔、整数、字符串）。
    pub value: Value,
}

/// `node.set_config` 参数。所有改动先校验，任何一项不合法就一项都不写。
///
/// 错误：键不在白名单、被环境变量覆盖、想放宽只能收紧的键 → `forbidden`（`data.field` 是键，`data.reason` 是
/// `not_editable` / `env_override` / `tighten_only`）；值的类型或取值不合法、键重复、`changes` 为空 → `invalid`（`data.field` 是键）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeSetConfigParams {
    pub changes: Vec<ConfigChange>,
}

/// 日志级别，`node.logs` 的最低级别过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// `node.logs` 参数。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeLogsParams {
    /// 最多返回多少行，缺省 200，最大 1000（超出按 1000）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    /// 最低级别：只返回这个级别及更高级别的行。缺省 = 不过滤。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<NodeLogLevel>,
}

/// `node.logs` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeLogsResult {
    /// 日志行，旧的在前、新的在后；每行最多 2 KiB（超出的截断，行尾加 `…`）。
    pub lines: Vec<String>,
    /// 还有更早的行没有返回（行数到了上限，或扫描的日志量到了上限）。
    pub truncated: bool,
}

/// 空参数 / 空结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Empty {}
