//! 到 daemon 的 JSON-RPC 连接：本机 Unix socket（明文换行分隔）或经 hub 的 Noise 通道（见 `remote.rs`）。
//! 对命令层是同一个接口：`call` 发请求等响应，`notifications` 收 D→C 通知。
//!
//! - 本机 socket 的读由**独立的读任务**完成（`read_until` 不可取消，所以绝不放进 `select!`），解析好的消息经 channel 交给
//!   驱动循环；长 notification 正在读的时候来了新请求不会丢数据；
//! - 每个请求有超时（默认 30 秒，`--timeout` 改；少数本来就慢的调用单独放宽）；
//! - 连接结束的原因记在 `closed` 里，请求失败时带上它（「被另一个进程顶掉了」比「连接丢了」有用得多）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_protocol::{RpcError, RpcId, RpcMessage, RpcNotification};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

use crate::error::{CliError, Result, EXIT_OFFLINE};

/// 默认的单个请求超时。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// 本机 socket 一行的上限。
const MAX_LINE: usize = 32 * 1024 * 1024;

pub(crate) struct Req {
    pub method: String,
    pub params: Value,
    pub reply: oneshot::Sender<std::result::Result<Value, RpcError>>,
}

/// 连接结束的原因（驱动循环退出时写入）。
pub(crate) type ClosedReason = Arc<Mutex<Option<String>>>;

pub struct Conn {
    tx: mpsc::UnboundedSender<Req>,
    notes: Option<mpsc::UnboundedReceiver<RpcNotification>>,
    pub local: bool,
    /// 远程连接的节点名（本机为 `None`）。
    pub node_name: Option<String>,
    closed: ClosedReason,
    timeout: Duration,
}

impl Conn {
    pub(crate) fn from_parts(
        tx: mpsc::UnboundedSender<Req>,
        notes: mpsc::UnboundedReceiver<RpcNotification>,
        local: bool,
        node_name: Option<String>,
        closed: ClosedReason,
    ) -> Conn {
        Conn { tx, notes: Some(notes), local, node_name, closed, timeout: DEFAULT_TIMEOUT }
    }

    pub fn set_timeout(&mut self, d: Duration) {
        self.timeout = d;
    }

    /// 单个请求的超时（`--timeout`）。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    fn lost(&self) -> CliError {
        let why = self.closed.lock().unwrap().clone();
        CliError::offline(why.unwrap_or_else(|| "the connection to the node was lost".into()))
    }

    /// 发请求并等响应，最多等 `timeout`。
    pub async fn call_timeout(&self, method: &str, params: impl Serialize, timeout: Duration) -> Result<Value> {
        let (reply, rx) = oneshot::channel();
        let params = serde_json::to_value(params)?;
        self.tx.send(Req { method: method.to_string(), params, reply }).map_err(|_| self.lost())?;
        match tokio::time::timeout(timeout, rx).await {
            Err(_) => Err(CliError::new(
                EXIT_OFFLINE,
                "timeout",
                format!("no answer to {method} within {} s; is the node busy or unreachable?", timeout.as_secs()),
            )),
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(e.into()),
            Ok(Err(_)) => Err(self.lost()),
        }
    }

    pub async fn call_raw(&self, method: &str, params: impl Serialize) -> Result<Value> {
        self.call_timeout(method, params, self.timeout).await
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: impl Serialize) -> Result<T> {
        let v = self.call_raw(method, params).await?;
        serde_json::from_value(v).map_err(|e| CliError::general(format!("unexpected response to {method}: {e}")))
    }

    /// 本来就慢的调用（探测、诊断）：超时放宽。
    pub async fn call_slow<T: DeserializeOwned>(&self, method: &str, params: impl Serialize) -> Result<T> {
        let v = self.call_timeout(method, params, self.timeout.max(Duration::from_secs(150))).await?;
        serde_json::from_value(v).map_err(|e| CliError::general(format!("unexpected response to {method}: {e}")))
    }

    /// 取走通知接收端（只能取一次）。
    pub fn notifications(&mut self) -> mpsc::UnboundedReceiver<RpcNotification> {
        self.notes.take().expect("notifications already taken")
    }
}

/// 读任务：一行一行读，解析成消息交给驱动循环。读任务自己不参与 `select!`，所以不会丢半行数据。
/// 文件结束 / 出错 / 行超长时发 `None` 并退出。
async fn read_task(r: tokio::net::unix::OwnedReadHalf, out: mpsc::UnboundedSender<Option<RpcMessage>>) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.len() > MAX_LINE {
            break;
        }
        let line = buf.trim_ascii();
        if line.is_empty() {
            continue;
        }
        match RpcMessage::decode(line) {
            Ok(m) => {
                if out.send(Some(m)).is_err() {
                    return;
                }
            }
            Err(e) => tracing::debug!(error = %e, "unparseable line from the daemon"),
        }
    }
    let _ = out.send(None);
}

/// 连本机 daemon 的 socket。socket 不存在 / 连不上时按是否已初始化区分错误。
pub async fn connect_local(home: &Path) -> Result<Conn> {
    let paths = anycon_daemon::config::Paths::new(home);
    let stream = match UnixStream::connect(paths.sock()).await {
        Ok(s) => s,
        Err(e) => {
            if !paths.config_toml().exists() {
                return Err(CliError::not_initialized(format!(
                    "{} is not initialized; run `anycon init` first",
                    home.display()
                )));
            }
            return Err(CliError::offline(format!(
                "the daemon is not running ({e}); start it with `anycon service start` or `anycon daemon`"
            )));
        }
    };
    let (r, mut w) = stream.into_split();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Req>();
    let (note_tx, note_rx) = mpsc::unbounded_channel::<RpcNotification>();
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<Option<RpcMessage>>();
    let closed: ClosedReason = Arc::new(Mutex::new(None));
    tokio::spawn(read_task(r, line_tx));
    let closed_w = closed.clone();
    tokio::spawn(async move {
        let mut pending: HashMap<u64, oneshot::Sender<std::result::Result<Value, RpcError>>> = HashMap::new();
        let mut next = 0u64;
        let reason = loop {
            // 两个分支都是 channel 的 recv，取消安全
            tokio::select! {
                msg = line_rx.recv() => {
                    match msg {
                        Some(Some(RpcMessage::Response(r))) => {
                            if let RpcId::Num(id) = r.id {
                                if let Some(tx) = pending.remove(&id) {
                                    let _ = tx.send(match r.error { Some(e) => Err(e), None => Ok(r.result.unwrap_or(Value::Null)) });
                                }
                            }
                        }
                        Some(Some(RpcMessage::Notification(n))) => { let _ = note_tx.send(n); }
                        Some(Some(_)) => {}
                        Some(None) | None => break "the daemon closed the connection",
                    }
                }
                req = req_rx.recv() => {
                    let Some(req) = req else { break "closed locally" };
                    next += 1;
                    let msg = RpcMessage::request(next, req.method, req.params);
                    let mut bytes = msg.encode();
                    bytes.push(b'\n');
                    if w.write_all(&bytes).await.is_err() { break "writing to the daemon socket failed"; }
                    pending.insert(next, req.reply);
                }
            }
        };
        // 连接结束：未完成的请求以「连接丢失」收场（reply 被 drop，调用方得到带原因的 offline 错误）
        *closed_w.lock().unwrap() = Some(format!("the connection to the daemon was lost ({reason})"));
        drop(pending);
    });
    Ok(Conn::from_parts(req_tx, note_rx, true, None, closed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    fn setup() -> (tempfile::TempDir, UnixListener) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "").unwrap();
        let l = UnixListener::bind(d.path().join("daemon.sock")).unwrap();
        (d, l)
    }

    #[tokio::test]
    async fn a_request_sent_while_a_long_notification_is_half_read_loses_nothing() {
        let (d, listener) = setup();
        let big = "n".repeat(3_000_000);
        let note = RpcMessage::Notification(RpcNotification {
            jsonrpc: "2.0".into(),
            method: "session.event".into(),
            params: serde_json::json!({ "text": big }),
        });
        let mut line = note.encode();
        line.push(b'\n');
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            // 先写半条通知，等一会儿（这时客户端会发出请求），再写后半条和响应
            let half = line.len() / 2;
            w.write_all(&line[..half]).await.unwrap();
            w.flush().await.unwrap();
            let mut reader = BufReader::new(r);
            let mut req = String::new();
            reader.read_line(&mut req).await.unwrap();
            let id = match RpcMessage::decode(req.trim().as_bytes()).unwrap() {
                RpcMessage::Request(r) => r.id,
                other => panic!("{other:?}"),
            };
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            w.write_all(&line[half..]).await.unwrap();
            let mut resp = RpcMessage::result(id, serde_json::json!({ "ok": true })).encode();
            resp.push(b'\n');
            w.write_all(&resp).await.unwrap();
            w.flush().await.unwrap();
            // 保持连接直到客户端读完
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        });
        let mut conn = connect_local(d.path()).await.unwrap();
        let mut notes = conn.notifications();
        // 请求在通知读到一半的时候发出（先让半条到达）
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let v = conn.call_raw("anything", serde_json::json!({})).await.unwrap();
        assert_eq!(v, serde_json::json!({ "ok": true }));
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), notes.recv()).await.unwrap().unwrap();
        assert_eq!(n.params["text"].as_str().unwrap().len(), 3_000_000, "the notification arrived whole");
        server.abort();
    }

    #[tokio::test]
    async fn requests_time_out_instead_of_hanging_and_a_lost_connection_says_why() {
        let (d, listener) = setup();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            // 收下请求但永远不答复，然后（第二个请求时）关掉连接
            let (r, w) = s.into_split();
            let mut reader = BufReader::new(r);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            drop(w);
            drop(reader);
        });
        let conn = connect_local(d.path()).await.unwrap();
        let e = conn
            .call_timeout("slow.method", serde_json::json!({}), std::time::Duration::from_millis(300))
            .await
            .unwrap_err();
        assert_eq!((e.exit, e.code.as_str()), (EXIT_OFFLINE, "timeout"));
        assert!(e.message.contains("slow.method"));
        // 服务端关连接：请求以离线收场，并带上原因
        let e =
            conn.call_timeout("second", serde_json::json!({}), std::time::Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(e.exit, EXIT_OFFLINE);
        assert!(e.message.contains("lost") || e.message.contains("closed"), "{}", e.message);
        server.abort();
    }
}
