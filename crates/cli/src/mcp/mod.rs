//! `anycon mcp`：stdio 上的 MCP server，让别的 agent 通过 MCP 工具操作这个节点上的会话（ADR-49）。
//!
//! 它只是又一个客户端入口：复用 CLI 的连接层（默认连本机 daemon，`--hub` / `--node` / `--local` 和别的子命令一致），
//! 把 MCP 工具调用翻译成 daemon 已有的 RPC。不调度、不在会话之间转发消息、不保存状态；编排逻辑在调用方。
//!
//! - [`protocol`]：MCP 的 JSON-RPC 2.0 协议层（手写，没有 SDK 依赖）；
//! - [`tools`]：九个工具和安全边界（不暴露权限审批、拒绝危险权限模式、防递归）。
//!
//! stdout 只写协议消息，其余输出一律走 stderr。daemon 连不上不会让 server 退出：工具调用返回错误，下一次调用重连。
//! 本机连接连上之后先 `local.set_client { client: "mcp" }`，daemon 的审计里据此记成 `local-mcp`（和人敲的 `local-cli` 分开）。

pub mod protocol;
pub mod tools;

use std::sync::Arc;

use anycon_daemon::env::AGENT_MARKER_ENV;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::cli::McpArgs;
use crate::commands::App;
use crate::conn::Conn;
use crate::error::{CliError, Result, EXIT_OFFLINE};

use tools::{AnyconTools, Node, Options};

/// 到 daemon 的连接：用到才连，断了下次调用重连。
struct ConnNode {
    app: Arc<App>,
    conn: Mutex<Option<Arc<Conn>>>,
}

impl ConnNode {
    async fn connect(&self) -> Result<Arc<Conn>> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        let mut conn = self.app.conn().await?;
        // 通知（`session.changed`、`permission.request`…）发给所有在线客户端，不读会一直堆在队列里：读掉丢弃
        let mut notes = conn.notifications();
        tokio::spawn(async move { while notes.recv().await.is_some() {} });
        if conn.local {
            // 老 daemon 没有这个方法：审计里仍记成 local-cli，不影响使用。经 hub 的连接不声明（身份由配对的设备决定）
            if let Err(e) = conn.call_raw(anycon_daemon::local_methods::SET_CLIENT, json!({ "client": "mcp" })).await {
                eprintln!("anycon mcp: could not tell the daemon this is an MCP client ({e}); audit entries will say local-cli");
            }
        }
        let conn = Arc::new(conn);
        *slot = Some(conn.clone());
        Ok(conn)
    }
}

impl Node for ConnNode {
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let conn = self.connect().await?;
        let r = conn.call_raw(method, params).await;
        if let Err(e) = &r {
            // 连接丢了（不是单个请求超时）：丢掉缓存的连接，下一次调用重连
            if e.exit == EXIT_OFFLINE && e.code != "timeout" {
                let mut slot = self.conn.lock().await;
                if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                    *slot = None;
                }
            }
        }
        r
    }
}

/// 这个进程是不是在 anycon 拉起的 agent 里（环境里有 `ANYCON_AGENT`）。
fn started_inside_a_session() -> bool {
    std::env::var_os(AGENT_MARKER_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

pub async fn run(app: &App, a: McpArgs) -> Result<u8> {
    let nested = started_inside_a_session();
    let opts = Options { allow_dangerous_modes: a.allow_dangerous_modes, block_mutations: nested && !a.allow_nested };
    if opts.block_mutations {
        eprintln!(
            "anycon mcp: running inside an anycon agent session ({AGENT_MARKER_ENV} is set): create_session, send_message and cancel_turn are disabled; pass --allow-nested to allow them"
        );
    }
    if opts.allow_dangerous_modes {
        eprintln!("anycon mcp: --allow-dangerous-modes: sessions may be created in permission modes that run tools without asking");
    }
    let node = ConnNode { app: Arc::new(app.clone()), conn: Mutex::new(None) };
    let host = Arc::new(AnyconTools::new(node, opts));
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    protocol::serve(host, stdin, tokio::io::stdout())
        .await
        .map_err(|e| CliError::general(format!("MCP stdio failed: {e}")))?;
    Ok(0)
}
