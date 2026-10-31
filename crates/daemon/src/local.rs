//! 本机通道：每行一条 JSON-RPC，方法与内层协议相同，不加密（ADR-27）。
//!
//! - unix：Unix socket `~/.anycon/daemon.sock`，权限 0600
//! - Windows：命名管道 `\\.\pipe\anycon-<hash>`（[`crate::config::Paths::sock`]）。管道的属主是当前用户，DACL 只允许当前用户，
//!   拒绝远程客户端；第一个实例带 `FILE_FLAG_FIRST_PIPE_INSTANCE`，同名管道已经存在时 daemon 起不来，而不是和别人共用一个名字。
//!   管道名字是全局的，别的本机用户可以抢先建一个同名管道等 CLI 连上来，所以客户端（[`connect`]）连上之后先核对管道的属主
//!   是不是当前用户，不是就断开
//!
//! 本机连接和 Noise 对端一样是「客户端」：可以 `session.watch` 收事件，收 `permission.request` 等通知。

use std::path::Path;
use std::sync::Arc;

use anycon_protocol::{RpcMessage, RpcNotification, RpcRequest};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
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

/// 监听中的本机通道。
pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    inner: pipe::PipeListener,
}

/// 绑定本机通道：unix 先清掉残留的 socket 文件，设为 0600；Windows 建第一个管道实例。
pub fn bind(path: &Path) -> std::io::Result<Listener> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let l = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener { inner: l })
    }
    #[cfg(windows)]
    {
        Ok(Listener { inner: pipe::PipeListener::bind(path)? })
    }
}

/// 收尾时删掉 socket 文件（命名管道随最后一个句柄消失，不用删）。
pub fn cleanup(path: &Path) {
    if cfg!(unix) {
        let _ = std::fs::remove_file(path);
    }
}

/// 服务端的一条连接。
#[cfg(unix)]
pub type ServerStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type ServerStream = tokio::net::windows::named_pipe::NamedPipeServer;

impl Listener {
    /// 等下一个客户端。
    pub async fn accept(&mut self) -> std::io::Result<ServerStream> {
        #[cfg(unix)]
        {
            self.inner.accept().await.map(|(s, _)| s)
        }
        #[cfg(windows)]
        {
            self.inner.accept().await
        }
    }
}

/// 客户端的一条连接（[`connect`]）。
#[cfg(unix)]
pub type ClientStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type ClientStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// 连本机通道。Windows 上核对管道属主是当前用户（模块说明），不是就返回 `PermissionDenied`。
pub async fn connect(path: &Path) -> std::io::Result<ClientStream> {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(path).await
    }
    #[cfg(windows)]
    {
        pipe::connect(path).await
    }
}

pub async fn serve(mut listener: Listener, ctx: Arc<Ctx>, rpc: Arc<Rpc>, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            r = listener.accept() => {
                match r {
                    Ok(stream) => {
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

async fn handle_conn<S: AsyncRead + AsyncWrite + Send + 'static>(stream: S, ctx: Arc<Ctx>, rpc: Arc<Rpc>) {
    let (r, mut w) = tokio::io::split(stream);
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

#[cfg(windows)]
mod pipe {
    //! Windows 命名管道：服务端的实例轮换、DACL 和属主，客户端的属主核对。

    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions};
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_PIPE_BUSY};
    use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES};

    use crate::sys::win::{current_user_sid, SecurityDescriptor};

    /// 属主和唯一的访问者都是当前用户。
    pub(super) fn pipe_sddl(sid: &str) -> String {
        format!("O:{sid}D:P(A;;GA;;;{sid})")
    }

    pub(super) struct PipeListener {
        name: PathBuf,
        sd: SecurityDescriptor,
        /// 下一个等待客户端的实例。
        next: NamedPipeServer,
    }

    impl PipeListener {
        pub(super) fn bind(name: &Path) -> std::io::Result<PipeListener> {
            let sd = SecurityDescriptor::from_sddl(&pipe_sddl(&current_user_sid()?))?;
            let next = create(name, &sd, true)?;
            Ok(PipeListener { name: name.to_path_buf(), sd, next })
        }

        /// 等一个客户端连上，并立刻建好下一个实例（期间来的客户端在 `connect` 里重试 `ERROR_PIPE_BUSY`）。
        pub(super) async fn accept(&mut self) -> std::io::Result<NamedPipeServer> {
            let connected = self.next.connect().await;
            let fresh = create(&self.name, &self.sd, false)?;
            let server = std::mem::replace(&mut self.next, fresh);
            connected.map(|_| server)
        }
    }

    fn create(name: &Path, sd: &SecurityDescriptor, first: bool) -> std::io::Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.as_ptr(),
            bInheritHandle: 0,
        };
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first).reject_remote_clients(true).pipe_mode(PipeMode::Byte);
        // SAFETY: `sa` 和它指向的安全描述符在调用期间有效（系统在创建时复制一份）。
        unsafe {
            opts.create_with_security_attributes_raw(name, (&mut sa as *mut SECURITY_ATTRIBUTES).cast::<c_void>())
        }
    }

    pub(super) async fn connect(name: &Path) -> std::io::Result<NamedPipeClient> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let client = loop {
            match ClientOptions::new().open(name) {
                Ok(c) => break c,
                // 所有实例都在忙（服务端刚接受了一个、还没建好下一个）：稍等再试
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        };
        let owner = pipe_owner_sid(&client)?;
        let me = current_user_sid()?;
        if owner != me {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is owned by another user ({owner}); refusing to talk to it", name.display()),
            ));
        }
        Ok(client)
    }

    /// 管道对象的属主 SID。
    fn pipe_owner_sid(client: &NamedPipeClient) -> std::io::Result<String> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: 句柄有效（以读写打开，含 READ_CONTROL）；成功时 `sd` 由系统分配，`owner` 指向它内部。
        let rc = unsafe {
            GetSecurityInfo(
                client.as_raw_handle(),
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        let mut s: *mut u16 = std::ptr::null_mut();
        // SAFETY: `owner` 在 `sd` 释放之前有效；成功时 `s` 由系统分配。
        let ok = unsafe { ConvertSidToStringSidW(owner, &mut s) } != 0;
        let out = if ok {
            // SAFETY: `s` 以 NUL 结尾。
            let text = unsafe {
                let n = (0..).take_while(|&i| *s.add(i) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(s, n))
            };
            // SAFETY: 由 ConvertSidToStringSidW 分配。
            unsafe { LocalFree(s.cast()) };
            Ok(text)
        } else {
            Err(std::io::Error::last_os_error())
        };
        // SAFETY: 由 GetSecurityInfo 分配。
        unsafe { LocalFree(sd) };
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        #[test]
        fn owner_and_dacl_are_the_user() {
            assert_eq!(pipe_sddl("S-1-5-21-9"), "O:S-1-5-21-9D:P(A;;GA;;;S-1-5-21-9)");
        }

        #[tokio::test]
        async fn round_trip_and_second_listener_is_refused() {
            let tmp = tempfile::tempdir().unwrap();
            let name = crate::config::Paths::new(tmp.path()).sock();
            let mut l = PipeListener::bind(&name).unwrap();
            assert!(PipeListener::bind(&name).is_err(), "the first instance owns the name");
            let srv = tokio::spawn(async move {
                let mut s = l.accept().await.unwrap();
                let mut buf = [0u8; 4];
                s.read_exact(&mut buf).await.unwrap();
                s.write_all(&buf).await.unwrap();
                // 第二个客户端也能连上（下一个实例已经建好）
                let _second = l.accept().await.unwrap();
            });
            let mut c = connect(&name).await.unwrap();
            c.write_all(b"ping").await.unwrap();
            let mut buf = [0u8; 4];
            c.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
            let _c2 = connect(&name).await.unwrap();
            srv.await.unwrap();
        }
    }
}
