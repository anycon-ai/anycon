//! 测试里起的最小 SMTP 服务：够 lettre 发信用（EHLO / AUTH / MAIL / RCPT / DATA / QUIT），记下收到的东西。
//! e2e 里的 Node 版本（`e2e/helpers/`）说的是同一套对话。
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[derive(Debug, Clone, Default)]
pub struct ReceivedMail {
    pub mail_from: String,
    pub rcpt_to: Vec<String>,
    /// DATA 的内容（已经去掉点填充，行尾 CRLF）。
    pub data: String,
}

impl ReceivedMail {
    /// 头部（DATA 里第一个空行之前）。
    pub fn headers(&self) -> &str {
        self.data.split_once("\r\n\r\n").map_or(self.data.as_str(), |(h, _)| h)
    }

    /// 取一个头的值（不区分大小写，不处理折行）。
    pub fn header(&self, name: &str) -> Option<String> {
        let prefix = format!("{}:", name.to_ascii_lowercase());
        self.headers()
            .split("\r\n")
            .find(|l| l.to_ascii_lowercase().starts_with(&prefix))
            .map(|l| l[prefix.len()..].trim().to_string())
    }

    /// 解开传输编码之后的正文文本。
    pub fn text(&self) -> String {
        let body = self.data.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        match self.header("content-transfer-encoding").unwrap_or_default().to_ascii_lowercase().as_str() {
            "base64" => {
                let joined: String = body.split("\r\n").collect();
                String::from_utf8_lossy(
                    &base64::engine::general_purpose::STANDARD.decode(joined.trim()).unwrap_or_default(),
                )
                .into_owned()
            }
            "quoted-printable" => decode_qp(body),
            _ => body.to_string(),
        }
    }

    /// 正文里的 6 位验证码（`123 456`）。
    pub fn code(&self) -> String {
        let text = self.text();
        let b = text.as_bytes();
        for i in 0..b.len().saturating_sub(6) {
            let w = &b[i..i + 7];
            if w[..3].iter().all(u8::is_ascii_digit) && w[3] == b' ' && w[4..].iter().all(u8::is_ascii_digit) {
                return format!("{}{}", &text[i..i + 3], &text[i + 4..i + 7]);
            }
        }
        panic!("no code in {text:?}");
    }
}

fn decode_qp(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' {
            if b.get(i + 1) == Some(&b'\r') && b.get(i + 2) == Some(&b'\n') {
                i += 3;
                continue;
            }
            if let (Some(h), Some(l)) = (b.get(i + 1), b.get(i + 2)) {
                if let Ok(v) = u8::from_str_radix(&format!("{}{}", *h as char, *l as char), 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Default)]
struct Shared {
    mails: Mutex<Vec<ReceivedMail>>,
    auths: Mutex<Vec<String>>,
    commands: Mutex<Vec<String>>,
    ehlo_names: Mutex<Vec<String>>,
    /// 对 RCPT TO 的回复；`None` = `250 ok`。
    rcpt_reply: Mutex<Option<String>>,
    advertise_auth: bool,
    advertise_starttls: bool,
}

pub struct FakeSmtp {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
}

impl FakeSmtp {
    /// `auth`：EHLO 里是否宣告 `AUTH PLAIN LOGIN`。
    pub async fn start(auth: bool) -> Self {
        Self::start_with(auth, false).await
    }

    /// `starttls`：EHLO 里是否宣告 `STARTTLS`（宣告了但真的发 STARTTLS 会得到 454，因为这个假服务不会 TLS）。
    pub async fn start_with(auth: bool, starttls: bool) -> Self {
        let shared = Arc::new(Shared { advertise_auth: auth, advertise_starttls: starttls, ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let s = shared.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let s = s.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, s).await;
                });
            }
        });
        Self { addr, shared }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn mails(&self) -> Vec<ReceivedMail> {
        self.shared.mails.lock().unwrap().clone()
    }

    /// 收到的 AUTH 凭据（解码后的 `authzid\0user\0pass` 里的 `user:pass`，或 LOGIN 的 `user:pass`）。
    pub fn auths(&self) -> Vec<String> {
        self.shared.auths.lock().unwrap().clone()
    }

    /// 收到的全部命令（大写动词）。
    pub fn commands(&self) -> Vec<String> {
        self.shared.commands.lock().unwrap().clone()
    }

    /// EHLO / HELO 里报的名字。
    pub fn ehlo_names(&self) -> Vec<String> {
        self.shared.ehlo_names.lock().unwrap().clone()
    }

    pub fn set_rcpt_reply(&self, reply: Option<&str>) {
        *self.shared.rcpt_reply.lock().unwrap() = reply.map(str::to_string);
    }

    /// 等到至少收到 `n` 封信（最多 5 秒）。
    pub async fn wait_for(&self, n: usize) -> Vec<ReceivedMail> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let got = self.mails();
            if got.len() >= n {
                return got;
            }
            assert!(tokio::time::Instant::now() < deadline, "expected {n} mail(s), got {}", got.len());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn serve(stream: tokio::net::TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    w.write_all(b"220 fake.smtp ESMTP ready\r\n").await?;
    let mut current = ReceivedMail::default();
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let cmd = line.trim_end().to_string();
        let upper = cmd.to_ascii_uppercase();
        let verb = upper.split_whitespace().next().unwrap_or("").to_string();
        shared.commands.lock().unwrap().push(verb.clone());
        match verb.as_str() {
            "EHLO" | "HELO" => {
                shared.ehlo_names.lock().unwrap().push(cmd.split_whitespace().nth(1).unwrap_or("").to_string());
                let mut reply = String::from("250-fake.smtp\r\n250-8BITMIME\r\n");
                if shared.advertise_starttls {
                    reply.push_str("250-STARTTLS\r\n");
                }
                if shared.advertise_auth {
                    reply.push_str("250-AUTH PLAIN LOGIN\r\n");
                }
                reply.push_str("250 SIZE 10485760\r\n");
                w.write_all(reply.as_bytes()).await?;
            }
            "AUTH" => {
                let parts: Vec<&str> = cmd.split_whitespace().collect();
                let b64 = base64::engine::general_purpose::STANDARD;
                let read_b64 = async |r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>| -> std::io::Result<String> {
                    let mut l = String::new();
                    r.read_line(&mut l).await?;
                    Ok(String::from_utf8_lossy(&b64.decode(l.trim()).unwrap_or_default()).into_owned())
                };
                match parts.get(1).map(|m| m.to_ascii_uppercase()).as_deref() {
                    Some("PLAIN") => {
                        let creds = match parts.get(2) {
                            Some(p) => String::from_utf8_lossy(&b64.decode(p).unwrap_or_default()).into_owned(),
                            None => {
                                w.write_all(b"334 \r\n").await?;
                                read_b64(&mut r).await?
                            }
                        };
                        let mut it = creds.split('\0').skip(1);
                        shared.auths.lock().unwrap().push(format!(
                            "{}:{}",
                            it.next().unwrap_or(""),
                            it.next().unwrap_or("")
                        ));
                        w.write_all(b"235 2.7.0 authenticated\r\n").await?;
                    }
                    Some("LOGIN") => {
                        w.write_all(b"334 VXNlcm5hbWU6\r\n").await?;
                        let user = read_b64(&mut r).await?;
                        w.write_all(b"334 UGFzc3dvcmQ6\r\n").await?;
                        let pass = read_b64(&mut r).await?;
                        shared.auths.lock().unwrap().push(format!("{user}:{pass}"));
                        w.write_all(b"235 2.7.0 authenticated\r\n").await?;
                    }
                    _ => w.write_all(b"504 unsupported mechanism\r\n").await?,
                }
            }
            "STARTTLS" => w.write_all(b"454 TLS not available\r\n").await?,
            "MAIL" => {
                current = ReceivedMail { mail_from: bracketed(&cmd), ..Default::default() };
                w.write_all(b"250 ok\r\n").await?;
            }
            "RCPT" => {
                let reply = shared.rcpt_reply.lock().unwrap().clone();
                match reply {
                    Some(r) => w.write_all(format!("{r}\r\n").as_bytes()).await?,
                    None => {
                        current.rcpt_to.push(bracketed(&cmd));
                        w.write_all(b"250 ok\r\n").await?;
                    }
                }
            }
            "DATA" => {
                w.write_all(b"354 end with <CRLF>.<CRLF>\r\n").await?;
                let mut data = String::new();
                loop {
                    let mut l = String::new();
                    if r.read_line(&mut l).await? == 0 {
                        return Ok(());
                    }
                    if l == ".\r\n" {
                        break;
                    }
                    data.push_str(l.strip_prefix('.').filter(|_| l.starts_with("..")).unwrap_or(&l));
                }
                current.data = data;
                shared.mails.lock().unwrap().push(std::mem::take(&mut current));
                w.write_all(b"250 2.0.0 queued\r\n").await?;
            }
            "RSET" | "NOOP" => w.write_all(b"250 ok\r\n").await?,
            "QUIT" => {
                w.write_all(b"221 bye\r\n").await?;
                return Ok(());
            }
            _ => w.write_all(b"500 unrecognised command\r\n").await?,
        }
    }
}

/// `MAIL FROM:<a@b>` / `RCPT TO:<a@b> ...` 里尖括号中的地址。
fn bracketed(cmd: &str) -> String {
    match (cmd.find('<'), cmd.find('>')) {
        (Some(a), Some(b)) if b > a => cmd[a + 1..b].to_string(),
        _ => String::new(),
    }
}
