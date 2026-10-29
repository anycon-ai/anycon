//! daemon 核心库。`anycon_daemon::run` 启动整个 daemon；CLI 的 `anycon daemon` 只是它的薄封装。
//!
//! 主要模块：
//! - `daemon` / `local` / `rpc`：装配与生命周期、本机通道（Unix socket / Windows 命名管道）、RPC 调度
//! - `hub` / `peers` / `flow`：到 hub 的连接、已配对客户端的 Noise 会话、发送方流控
//! - `sessions` / `agents` / `workspace` / `worktree`：会话、agent 注册与探测、项目目录、每会话 worktree
//! - `store` / `config` / `audit` / `blobs`：SQLite 存储、配置、审计、附件通道
//! - `preview`：dev server 预览（票据、端口探测、Noise 隧道里的 HTTP / WebSocket 转发）
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
pub mod hub;
pub mod identity;
pub mod limits;
pub mod linkrules;
pub mod local;
pub mod logging;
pub mod notify;
pub mod peers;
pub mod preview;
pub mod rpc;
pub mod sessions;
pub mod setup;
pub mod store;
pub mod sys;
pub mod util;
pub mod workspace;
pub mod worktree;

#[cfg(any(test, feature = "testing"))]
pub mod testutil;

pub use daemon::{run, Ctx, Daemon, DaemonError, DaemonOptions};
pub use rpc::local_methods;
