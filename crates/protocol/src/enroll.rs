//! 账号内接入（ADR-36，docs/protocol.md § 4.4）。
//!
//! 已登录账号的新设备不用短码，经 hub 请求接入同账号下的一台 daemon；daemon 用扫码密钥、接入码或节点口令验证。
//! 两部分：
//! - [`EnrollFrame`]：接入期间 `handshake` 信封的 body（msgpack，内部标签 `type`）。hub 不解析它。
//!   前置消息（`hello` / `reply`）和 CPace 份额是明文，后面的 `noise` 帧装的是 `Noise_XXpsk3` 握手消息，
//!   PSK 来自 CPace（`code` / `password`）或二维码里的密钥（`scan`）。握手之后照配对收尾：`pair_confirm` → `pair_done` → `pair.ack`
//! - `node.enroll_*` 等内层方法的参数 / 结果：已配对设备批准 / 拒绝请求、生成接入码、管理节点口令

use serde::{Deserialize, Serialize};

use crate::{DeviceId, DeviceKind, TimestampMs};

/// `EnrollHello.v` 的当前值。
pub const ENROLL_FRAME_VERSION: u32 = 1;

/// 接入方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollMethod {
    /// 已配对设备扫新设备上的二维码，把二维码里的 32 字节密钥交给 daemon。
    Scan,
    /// 6 位接入码（批准请求时生成，或「添加设备」主动生成），走 CPace。
    Code,
    /// 节点口令，走 CPace。
    Password,
}

/// 新设备自报的提示信息。**未经认证**（hub 能改），只用于批准界面；认证过的 `ClientInfo` 以 XXpsk3 第 3 条为准。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EnrollHint {
    pub name: String,
    pub kind: DeviceKind,
    pub platform: String,
}

/// `EnrollReply.status`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollReplyStatus {
    /// 等已配对设备批准（`scan`、`code` 且 `approval = true`）。批准后 daemon 再发一条 `ready`。
    WaitApproval,
    /// 可以继续：`code` / `password` 带 daemon 的 CPace 份额（`password` 另带盐），`scan` 什么都不带。
    Ready,
    /// 拒绝，原因在 `reason`。
    Rejected,
}

/// `EnrollReply.reason`（`status = rejected` 时）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollRejectReason {
    /// `[enroll]` 配置关掉了这种方式。
    MethodDisabled,
    /// 没设节点口令。
    PasswordNotSet,
    /// 口令连续失败后的退避期，带 `retry_after_ms`。
    Backoff,
    /// 口令解锁已锁定（连续 10 次失败），要在本机或已配对设备上重新启用。
    Locked,
    /// `code` 且 `approval = false`，但节点上没有有效的主动接入码。
    NoCode,
    /// 进行中的接入已满（每节点 3 个），或已有一个口令尝试在进行。
    Busy,
    /// 已配对设备拒绝了请求。
    Denied,
    /// XXpsk3 第 3 条校验失败：码 / 口令不对（或有中间人）。码还有剩余次数时带 `attempts_left`。
    WrongSecret,
    /// 请求或接入码过期 / 次数用完。
    Expired,
    /// 帧格式或顺序不对。
    Invalid,
}

/// 接入期间 `handshake` 信封的 body。N = 新设备（initiator），D = daemon（responder）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnrollFrame {
    /// N→D：选择方法。`code` 同一会话里可以重发再试（每次一轮新的 CPace）。
    Hello {
        v: u32,
        method: EnrollMethod,
        /// 仅 `code`：`true` = 请求已配对设备批准；`false` = 用「添加设备」主动生成的码。
        #[serde(default)]
        approval: bool,
        /// 16 字节随机数，hex。CPace 的 sid = N 的 nonce ‖ D 的 nonce。
        nonce: String,
        hint: EnrollHint,
    },
    /// D→N
    Reply {
        status: EnrollReplyStatus,
        /// D 的 16 字节随机数，hex。每条 `ready` 都带（和 N 本轮 `hello` 的 nonce 组成 sid）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nonce: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<EnrollRejectReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempts_left: Option<u32>,
        /// 仅 `password`：节点盐（16 字节，hex）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        salt: Option<String>,
        /// `code` / `password`：D 的 CPace 份额（32 字节，hex）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cpace: Option<String>,
    },
    /// N→D：N 的 CPace 份额（32 字节，hex）。紧接着 N 发第一条 `noise`。
    Cpace { share: String },
    /// 双向：一条 `Noise_XXpsk3` 握手消息（hex）。N 是 initiator。
    Noise { msg: String },
}

// ---- 内层方法 ----

/// 进行中的接入会话的状态（daemon 视角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollSessionStatus {
    /// 收到 `enroll_request`，还没收到 `hello`。
    WaitingHello,
    /// 等已配对设备批准。
    WaitApproval,
    /// 已经可以握手（或正在握手）。
    Ready,
}

/// 一条进行中的接入（`node.enroll_list` 的元素、`node.enroll_requested` 的参数）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EnrollInfo {
    pub enroll_id: String,
    /// 还没收到 `hello` 时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<EnrollMethod>,
    #[serde(default)]
    pub approval: bool,
    /// 新设备自报，未经认证。还没收到 `hello` 时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<EnrollHint>,
    pub status: EnrollSessionStatus,
    pub created_at_ms: TimestampMs,
    pub expires_at_ms: TimestampMs,
}

/// `node.enroll_list` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeEnrollListResult {
    pub enrolls: Vec<EnrollInfo>,
}

/// `node.enroll_approve` 参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeEnrollApproveParams {
    pub enroll_id: String,
    /// `scan` 必填：二维码里的 32 字节密钥（hex）。`code` 不带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// `node.enroll_approve` 结果。`code` 请求返回 6 位码（只给批准的这一台）；`scan` 两个字段都为空。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeEnrollApproveResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<TimestampMs>,
}

/// `node.enroll_deny` 参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeEnrollIdParams {
    pub enroll_id: String,
}

/// `node.enroll_resolved.result`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollResult {
    Approved,
    Denied,
    Completed,
    Failed,
    Expired,
}

/// D→C notification `node.enroll_resolved`：发给所有在线的已配对设备，让别的设备收起批准卡片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeEnrollResolved {
    pub enroll_id: String,
    pub result: EnrollResult,
    /// 批准 / 拒绝的设备；daemon 自己判定的（过期、失败、完成）为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_device: Option<DeviceId>,
}

/// `node.enroll_code` 结果：主动生成的 6 位接入码。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeEnrollCodeResult {
    pub code: String,
    pub expires_at_ms: TimestampMs,
}

/// 节点口令解锁的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollPasswordState {
    Enabled,
    /// `enroll.password = false`。
    Disabled,
    /// 连续 10 次失败。
    Locked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EnrollPasswordStatus {
    /// 是否设了口令。
    pub set: bool,
    pub state: EnrollPasswordState,
    /// 连续失败次数。
    pub failures: u32,
    /// 退避中：还要等多久才接受下一次尝试。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_at_ms: Option<TimestampMs>,
    /// 设置者：设备 id，本机 CLI 设置的是 `"local"`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_by: Option<String>,
}

/// `node.enroll_status` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeEnrollStatusResult {
    /// `enroll.approval`：是否接受扫码批准和接入码。
    pub approval: bool,
    pub password: EnrollPasswordStatus,
}

/// `node.set_enroll_password` 参数。`password` 省略 = 清除。至少 8 个字符（按 Unicode 字符计，NFC 之后）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeSetEnrollPasswordParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// `node.set_enroll_enabled` 参数：写 `[enroll]` 配置。`password: true` 同时解除锁定、失败计数清零。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeSetEnrollEnabledParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<bool>,
}

/// `node.security_event.kind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecurityEventKind {
    /// 新设备经接入加入。
    DeviceEnrolled,
    /// 节点口令连续失败满 3 次。
    PasswordFailures,
    /// 口令解锁被锁定。
    PasswordLocked,
    /// 口令被设置 / 修改 / 清除。
    PasswordChanged,
    /// `[enroll]` 被修改（含重新启用）。
    SettingsChanged,
}

/// D→C notification `node.security_event`（同时发 `notify { category: security }`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeSecurityEvent {
    pub kind: SecurityEventKind,
    pub ts_ms: TimestampMs,
    /// 触发的已配对设备（改口令、改设置）；本机 CLI 为 `"local"`；接入、失败为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_device: Option<String>,
    /// `device_enrolled`：新设备（认证过的）名称和 id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<EnrollMethod>,
    /// `password_failures` / `password_locked`：当时的连续失败次数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failures: Option<u32>,
    /// `password_changed`：`true` = 设置 / 修改，`false` = 清除。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_set: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_msgpack_roundtrip() {
        let f = EnrollFrame::Hello {
            v: ENROLL_FRAME_VERSION,
            method: EnrollMethod::Code,
            approval: true,
            nonce: "00".repeat(16),
            hint: EnrollHint { name: "Chrome · Windows".into(), kind: DeviceKind::Web, platform: "browser".into() },
        };
        let bytes = rmp_serde::to_vec_named(&f).unwrap();
        let back: EnrollFrame = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, f);

        let r = EnrollFrame::Reply {
            status: EnrollReplyStatus::Rejected,
            nonce: None,
            reason: Some(EnrollRejectReason::Backoff),
            retry_after_ms: Some(60_000),
            attempts_left: None,
            salt: None,
            cpace: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["type"], "reply");
        assert_eq!(json["reason"], "backoff");
        assert!(json.get("salt").is_none());
    }
}
