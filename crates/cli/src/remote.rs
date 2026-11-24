//! CLI 作为客户端设备经 hub 连 daemon：传输、配对、设备身份的存储都在 `anycon_daemon::client`（Bot 设备共用），
//! 这里只是把它接到 CLI 上：设备种类固定是 `cli`、错误换成带退出码的 [`CliError`]、需要让用户看到的提示打印到 stderr、
//! 连上的节点交给 [`Conn`]。
//!
//! 每个 hub 一套状态，存在 `<home>/client/<hub-host>/`，文件格式见 `anycon_daemon::client` 的 `HubState`。

use std::time::Duration;

use anycon_daemon::config::Paths;
use anycon_protocol::{ClientInfo, DeviceKind};

pub use anycon_daemon::client::{
    check_protocol, host_of, list_hubs, refresh_node_name, ConnectFail, HubState, HubWs, NodeEntry, PairDoneInfo,
    ACK_TIMEOUT,
};

use crate::conn::Conn;
use crate::error::Result;

/// 这台设备在 hub 上自报的信息：种类是 `cli`。
pub fn client_info(name: &str) -> ClientInfo {
    anycon_daemon::client::client_info(name, DeviceKind::Cli)
}

/// 库里要给用户看的提示（比如 token 轮换后写不进磁盘）打印到 stderr。
fn print_notice(msg: &str) {
    eprintln!("{msg}");
}

fn init() {
    anycon_daemon::client::set_notice_sink(print_notice);
}

/// `anycon connect`：作为 joiner 配对。成功（`pair.ack` 校验通过）后才把节点记入本地状态。
pub async fn pair(paths: &Paths, hub_url: &str, code_arg: &str, device_name: &str) -> Result<NodeEntry> {
    init();
    Ok(anycon_daemon::client::pair(paths, hub_url, code_arg, client_info(device_name)).await?)
}

/// 配对的后半段（账号内接入在 `pair_done` 之后接上来）。
pub async fn complete_pairing(
    state: &mut HubState,
    h: &mut HubWs,
    pair_transport: &mut anycon_crypto::Transport,
    node_pub: [u8; 32],
    done: PairDoneInfo,
    ack_timeout: Duration,
) -> Result<NodeEntry> {
    init();
    Ok(anycon_daemon::client::complete_pairing(state, h, pair_transport, node_pub, done, ack_timeout).await?)
}

pub fn pair_failed(reason: anycon_protocol::PairFailReason) -> crate::error::CliError {
    anycon_daemon::client::pair_failed(reason).into()
}

/// 打开到某个节点的远程连接。
pub async fn open(state: &HubState, node: &NodeEntry, device_name: &str) -> Result<Conn> {
    init();
    let link = anycon_daemon::client::open(state, node, client_info(device_name)).await?;
    Ok(Conn::from_parts(link.tx, link.notes, false, Some(link.node_name), link.closed))
}

/// 节点在线状态（hub 的 `auth_ok.peers_online`）。连不上 hub 时返回 `None`。
pub async fn online_nodes(state: &HubState, device_name: &str) -> Option<Vec<String>> {
    anycon_daemon::client::online_nodes(state, client_info(device_name)).await
}
