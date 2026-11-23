//! 客户端传输：作为一台「设备」经 hub 连别的节点。CLI（每条命令一次连接）和分身（长期在线、同时连多个节点）共用。
//!
//! - [`state`]：设备身份（Noise 私钥、设备 id、device token）和已配对节点的本地存储，每个 hub 一个目录
//!   `<home>/client/<hub-host>/`，格式和位置不变；
//! - [`hubws`]：到 hub 的一条已认证连接，以及 token 轮换的落盘；
//! - [`pairing`]：短码配对的加入方（Noise XXpsk3 initiator，`ClientInfo` 由调用方给）和 `pair.ack` 的校验；
//! - [`channel`]：握手之后到一个节点的加密通道（分片、重组、发送方流控）；
//! - [`link`]：单节点的一次性连接（IK 握手 + 驱动循环），CLI 用；
//! - [`multi`]：[`Client`]——一个设备身份、一条 hub 连接、同时连多个节点，各自握手、各自重连。
//!
//! 错误文字写给终端用户看（英文），CLI 直接打印；机器可读的部分是 [`ClientError`] 的类别和错误码。

use std::sync::OnceLock;

mod channel;
mod error;
mod hubws;
mod link;
mod multi;
mod pairing;
mod state;

pub use error::{ClientError, ErrorClass, Result};
pub use hubws::{ConnectFail, HubWs, STEP};
pub use link::{check_protocol, online_nodes, open, ClosedReason, NodeLink, Req};
pub use multi::{Client, ClientEvent, ClientOptions, NodeStatus};
pub use pairing::{
    complete_pairing, finish_pairing, join_handshake, pair, pair_failed, pair_state, parse_code_arg, verify_pair_ack,
    PairDoneInfo, ACK_TIMEOUT,
};
pub use state::{
    client_info, host_of, is_invisible_format, list_hubs, refresh_node_name, sanitize_label, HubState, NodeEntry,
};

/// 需要让用户看到的提示（比如 token 轮换后写不进磁盘）的去处。缺省记 `warn` 日志；CLI 装上自己的（打印到 stderr），
/// 行为和原来一样。进程里只能装一次。
static NOTICE_SINK: OnceLock<fn(&str)> = OnceLock::new();

/// 装上提示的去处（只有第一次调用生效）。
pub fn set_notice_sink(f: fn(&str)) {
    let _ = NOTICE_SINK.set(f);
}

pub(crate) fn notice(msg: &str) {
    match NOTICE_SINK.get() {
        Some(f) => f(msg),
        None => tracing::warn!("{msg}"),
    }
}

#[cfg(test)]
mod tests;
