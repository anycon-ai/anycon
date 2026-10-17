//! 本机 Unix socket（`~/.anycon/daemon.sock`）：每行一条 JSON-RPC，方法与内层协议相同，不加密，权限 0600。
//!
//! 本机连接和 Noise 对端一样是「客户端」：可以 `session.watch` 收事件，收 `permission.request` 等通知。

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anycon_protocol::{RpcMessage, RpcNotification, RpcRequest};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

use crate::clients::{Caller, ClientSink};
use crate::daemon::Ctx;
use crate::rpc::{parse_error_response, parse_request, Rpc};

/// 单行上限（防止畸形客户端吃内存）。
const MAX_LINE: usize = 16 * 1024 * 1024;

struct LineSink(mpsc::UnboundedSender<Vec<u8>>);

impl ClientSink for LineSink {
    fn send(&self, n: RpcNotification) {
        let _ = self.0.send(RpcMessage::Notification(n).encode());
    }
}

/// 绑定 socket（先清掉残留文件）并设为 0600。
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

pub async fn serve(listener: UnixListener, ctx: Arc<Ctx>, rpc: Arc<Rpc>, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            r = listener.accept() => {
                match r {
                    Ok((stream, _)) => {
                        let (ctx, rpc) = (ctx.clone(), rpc.clone());
                        tokio::spawn(async move { handle_conn(stream, ctx, rpc).await });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "socket accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
            _ = shutdown.changed() => return,
        }
    }
}

async fn handle_conn(stream: UnixStream, ctx: Arc<Ctx>, rpc: Arc<Rpc>) {
    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let client = ctx.clients.register(Caller::local_cli(), Box::new(LineSink(tx.clone())));
    let mut writer = tokio::spawn(async move {
        while let Some(mut line) = rx.recv().await {
            line.push(b'\n');
            if w.write_all(&line).await.is_err() {
                return;
            }
        }
    });
    // 同一个连接的请求按到达顺序串行处理（protocol.md § 7.3）
    let (req_tx, req_rx) = mpsc::unbounded_channel::<RpcRequest>();
    let out_tx = tx.clone();
    let worker = crate::rpc::spawn_serial_worker(rpc.clone(), client.clone(), req_rx, move |resp| {
        let _ = out_tx.send(resp.encode());
    });
    let mut reader = BufReader::new(r);
    let mut line = Vec::new();
    loop {
        line.clear();
        // 最多读 MAX_LINE + 1 字节：超长的行不用读完整行就能发现
        match (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE {
            tracing::warn!("local socket: a request line exceeded {MAX_LINE} bytes; closing the connection");
            break;
        }
        let trimmed = line.trim_ascii();
        if trimmed.is_empty() {
            continue;
        }
        match parse_request(trimmed) {
            Ok(Some(req)) => {
                let _ = req_tx.send(req);
            }
            Ok(None) => {}
            Err(e) => {
                let _ = tx.send(parse_error_response(e).encode());
            }
        }
    }
    // 对端关了写端（EOF）也要把已经收到的请求答完再收尾
    ctx.clients.unregister(client.conn_id);
    drop(req_tx);
    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), worker).await;
    drop(tx);
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut writer).await.is_err() {
        writer.abort();
    }
}
