//! daemon 核心库。`anycon_daemon::run` 启动整个 daemon；CLI 的 `anycon daemon` 只是它的薄封装。
//!
//! 主要模块：
//! - `daemon` / `local` / `rpc`：装配与生命周期、本机通道（Unix socket / Windows 命名管道）、RPC 调度
//! - `hub` / `peers` / `flow`：到 hub 的连接、已配对客户端的 Noise 会话、发送方流控
//! - `sessions` / `agents` / `workspace` / `wsroots` / `worktree`：会话、agent 注册与探测、项目目录（多个 workspace 根）、每会话 worktree
//! - `store` / `config` / `audit` / `blobs`：SQLite 存储、配置、审计、附件通道
//! - `remote_config` / `logtail`：远程查看 / 修改节点配置（白名单，ADR-43）、远程读日志尾部（`node.logs`）
//! - `todos` / `todo_dispatch`：清单（`todo.*`，每个节点一份个人待办，§ 21）和派发（`todo.dispatch`，从一项开会话、关联、`activity`、「经过」，§ 22）
//! - `rules`：决策档案的规则（`rule.*`，每个节点一份，生效的规则注入派发出去的会话的系统提示，§ 23）
//! - `transfer`：`anycon export` / `import`（会话历史和附件的 zip 导出 / 导入，只走本机通道）
//! - `preview`：dev server 预览（票据、端口探测、Noise 隧道里的 HTTP / WebSocket 转发）
//! - `update`：更新检查（每天问一次 anycon.ai 有没有新版本，只提示、不下载，ADR-57）
//! - `sys`：平台差异的小工具（只给属主访问、文件身份、规范路径……）

pub mod agents;
pub mod audit;
pub mod blobs;
pub mod clients;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod enroll;
pub mod enroll_initiator;
pub mod env;
pub mod flow;
pub mod fsopen;
pub mod gitops;
mod https;
pub mod hub;
pub mod identity;
pub mod judge;
pub mod limits;
pub mod linkrules;
pub mod local;
pub mod logging;
pub mod logtail;
pub mod notify;
pub mod peers;
pub mod preview;
pub mod remote_config;
pub mod rpc;
pub mod rule_extract;
pub mod rules;
pub mod sessions;
pub mod setup;
pub mod store;
pub mod sys;
pub mod todo_dispatch;
pub mod todos;
pub mod transfer;
pub mod update;
pub mod util;
pub mod workfiles;
pub mod workspace;
pub mod worktree;
pub mod wsroots;

#[cfg(any(test, feature = "testing"))]
pub mod testutil;

pub use daemon::{run, Ctx, Daemon, DaemonError, DaemonOptions};
pub use rpc::local_methods;
