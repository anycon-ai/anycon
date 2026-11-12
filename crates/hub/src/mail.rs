//! 发信：`Mailer` trait、基于 lettre 的 SMTP 实现、测试用的记录型实现，以及固定的中英文邮件模板。
//!
//! [ADR-37](../../../docs/decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)：验证码和通知邮件走同一条通道
//! （`ANYCON_SMTP_URL`）。TLS 一律 rustls + 内置的 webpki 根证书（lettre 的 `ring` 实现），不依赖系统证书和 OpenSSL。
//!
//! 日志纪律（docs/spec/hub.md § 10）：邮箱地址不进日志，调用方只记 [`crate::util::email_tag`]（哈希前 8 位）；
//! 邮件正文（含验证码）不进日志。SMTP 报错里服务器可能回显收件人地址，所以记日志前先过 [`MailError::redacted`]。

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use lettre::message::header::ContentType;
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::{is_private_ip, SmtpConfig, SmtpSecurity};

/// 一封要发出的邮件（发件人由 [`Mailer`] 自己带）。`Debug` 不输出收件人和正文（里面有邮箱和验证码）。
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingMail {
    /// 收件人，规范化后的裸地址。
    pub to: String,
    pub subject: String,
    /// 纯文本正文。
    pub body: String,
    /// HTML 正文。有的话和纯文本一起作为 `multipart/alternative` 发出，不显示 HTML 的客户端用纯文本那份。
    pub html: Option<String>,
}

impl std::fmt::Debug for OutgoingMail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutgoingMail").field("body_len", &self.body.len()).finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    #[error("invalid recipient address")]
    BadAddress,
    #[error("cannot build the message: {0}")]
    Build(String),
    #[error("cannot set up the SMTP transport: {0}")]
    Setup(String),
    #[error("smtp error{}: {message}", status.as_deref().map(|s| format!(" ({s})")).unwrap_or_default())]
    Smtp { status: Option<String>, message: String },
    /// `smtp+plain://` 的主机名解析失败。
    #[error("cannot resolve the smtp+plain host: {0}")]
    Resolve(String),
    /// `smtp+plain://` 的主机名解析出了不是本机 / 内网的地址：不把明文邮件和凭据发过去。
    #[error("the smtp+plain host resolves to {addr}, which is not a local or private address; not sending unencrypted mail there")]
    NotLocal { addr: IpAddr },
}

impl MailError {
    /// 适合写进日志的描述：服务器回显的收件人地址换成 `<redacted>`，并截断。
    pub fn redacted(&self, recipient: &str) -> String {
        let mut s = self.to_string();
        if !recipient.is_empty() {
            // ASCII 小写不改变字节长度，可以拿小写串里的位置去切原串。
            let hay = s.to_ascii_lowercase();
            let needle = recipient.to_ascii_lowercase();
            let mut out = String::with_capacity(s.len());
            let mut last = 0;
            for (i, _) in hay.match_indices(&needle) {
                out.push_str(&s[last..i]);
                out.push_str("<redacted>");
                last = i + needle.len();
            }
            out.push_str(&s[last..]);
            s = out;
        }
        s.chars().take(200).collect()
    }

    /// SMTP 状态码（有的话），写进「发送结果」日志。
    pub fn status(&self) -> Option<&str> {
        match self {
            MailError::Smtp { status, .. } => status.as_deref(),
            _ => None,
        }
    }
}

#[async_trait]
pub trait Mailer: Send + Sync + 'static {
    async fn send(&self, mail: OutgoingMail) -> Result<(), MailError>;
}

/// 把 `smtp+plain://` 的主机名解析成 IP 地址。单独成 trait 是为了测试里换成假的（不依赖真 DNS，
/// 也能模拟「一个名字解析到公网」）。
#[async_trait]
pub trait HostResolver: Send + Sync + 'static {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>>;
}

/// 用系统解析器（`getaddrinfo`，所以 `/etc/hosts`、`*.localhost`、docker 的服务名都认）。IP 字面量直接返回，不查 DNS。
pub struct SystemResolver;

#[async_trait]
impl HostResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
        let h = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = h.parse::<IpAddr>() {
            return Ok(vec![ip.to_canonical()]);
        }
        let mut out: Vec<IpAddr> = Vec::new();
        for addr in tokio::net::lookup_host((h, port)).await? {
            let ip = addr.ip().to_canonical();
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
        Ok(out)
    }
}

/// 解析主机名最多等多久。
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
/// 一次 SMTP 对话（连接 + 全部命令）的超时。
const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

/// 真实的 SMTP 发信（lettre，tokio + rustls）。
pub struct SmtpMailer {
    backend: Backend,
    from: Mailbox,
}

enum Backend {
    /// `smtp://`（STARTTLS）和 `smtps://`（隐式 TLS）：lettre 自己连，并按主机名校验证书。
    Remote(AsyncSmtpTransport<Tokio1Executor>),
    /// `smtp+plain://`：每次发信前自己解析主机名，确认**每个**地址都是本机 / 内网，再把 IP 交给 lettre。
    Plain(PlainRelay),
}

/// `smtp+plain://` 的目标。不预先建传输：连谁要等每次发信前解析之后才知道。
struct PlainRelay {
    host: String,
    port: u16,
    hello: ClientId,
    credentials: Option<Credentials>,
    resolver: Arc<dyn HostResolver>,
}

impl PlainRelay {
    fn transport(&self, ip: IpAddr) -> AsyncSmtpTransport<Tokio1Executor> {
        // lettre 收到的是 IP 的字符串形式：`(&str, u16)` 做 `ToSocketAddrs` 时先当 IP 解析，不会再去查 DNS。
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(ip.to_string())
            .port(self.port)
            .timeout(Some(SMTP_TIMEOUT))
            .hello_name(self.hello.clone());
        if let Some(c) = &self.credentials {
            builder = builder.credentials(c.clone());
        }
        builder.build()
    }

    async fn send(&self, message: Message) -> Result<(), MailError> {
        let resolved = tokio::time::timeout(RESOLVE_TIMEOUT, self.resolver.resolve(&self.host, self.port)).await;
        let addrs = match resolved {
            Ok(Ok(addrs)) => addrs,
            Ok(Err(e)) => return Err(MailError::Resolve(e.to_string())),
            Err(_) => return Err(MailError::Resolve("timed out".into())),
        };
        if addrs.is_empty() {
            return Err(MailError::Resolve("no addresses".into()));
        }
        // 一个名字有多个地址时，**每一个**都得是本机 / 内网：只要混进一个公网地址，这个名字就不可信（它可能被改了解析）。
        if let Some(bad) = addrs.iter().find(|ip| !is_private_ip(**ip)) {
            return Err(MailError::NotLocal { addr: *bad });
        }
        let mut last = None;
        for ip in addrs {
            match self.transport(ip).send(message.clone()).await {
                Ok(_) => return Ok(()),
                // 服务器答话了（拒收、要认证……）：换个地址也一样，不再试。
                Err(e) if e.status().is_some() => return Err(smtp_error(&e)),
                // 没连上 / 对话中途断了：试下一个地址（比如 `localhost` 解析出 ::1 和 127.0.0.1，服务只听其中一个）。
                Err(e) => last = Some(smtp_error(&e)),
            }
        }
        Err(last.expect("at least one address"))
    }
}

fn smtp_error(e: &lettre::transport::smtp::Error) -> MailError {
    MailError::Smtp { status: e.status().map(|c| c.to_string()), message: e.to_string() }
}

impl SmtpMailer {
    /// 按 `ANYCON_SMTP_URL` 建传输。这里不连网：连接在第一次发信时才建（lettre 的连接池按需建立）。
    pub fn new(cfg: &SmtpConfig, from: Mailbox) -> Result<Self, MailError> {
        Self::with_resolver(cfg, from, Arc::new(SystemResolver))
    }

    /// 同 [`SmtpMailer::new`]，`smtp+plain://` 的主机名用给定的解析器解析（测试注入）。
    pub fn with_resolver(cfg: &SmtpConfig, from: Mailbox, resolver: Arc<dyn HostResolver>) -> Result<Self, MailError> {
        let setup = |e: lettre::transport::smtp::Error| MailError::Setup(e.to_string());
        // EHLO 报发件人的域名：没开 lettre 的 `hostname` 特性（容器里取不到有意义的主机名），默认会报 `localhost`，
        // 有些中继会拒绝。
        let hello = ClientId::Domain(from.email.domain().to_string());
        let credentials =
            cfg.username.as_ref().map(|user| Credentials::new(user.clone(), cfg.password.clone().unwrap_or_default()));
        let backend = match cfg.security {
            SmtpSecurity::Plain => {
                Backend::Plain(PlainRelay { host: cfg.host.clone(), port: cfg.port, hello, credentials, resolver })
            }
            security => {
                let builder = match security {
                    // `starttls_relay`：STARTTLS 必须成功，服务器不支持就发不出去，不会退回明文。
                    SmtpSecurity::StartTls => {
                        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host).map_err(setup)?
                    }
                    _ => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host).map_err(setup)?,
                };
                let mut builder = builder.port(cfg.port).timeout(Some(SMTP_TIMEOUT)).hello_name(hello);
                if let Some(c) = credentials {
                    builder = builder.credentials(c);
                }
                Backend::Remote(builder.build())
            }
        };
        Ok(Self { backend, from })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, mail: OutgoingMail) -> Result<(), MailError> {
        let to: Mailbox = mail.to.parse().map_err(|_| MailError::BadAddress)?;
        // Message-ID 用发件人域名（没有它不少收件方会判成垃圾邮件）；Date 由 lettre 自己加。
        let message_id = format!("<{}@{}>", crate::util::random_hex(12), self.from.email.domain());
        let builder =
            Message::builder().message_id(Some(message_id)).from(self.from.clone()).to(to).subject(mail.subject);
        let message = match mail.html {
            Some(html) => builder.multipart(MultiPart::alternative_plain_html(mail.body, html)),
            None => builder.header(ContentType::TEXT_PLAIN).body(mail.body),
        }
        .map_err(|e| MailError::Build(e.to_string()))?;
        match &self.backend {
            Backend::Remote(transport) => transport.send(message).await.map(|_| ()).map_err(|e| smtp_error(&e)),
            Backend::Plain(relay) => relay.send(message).await,
        }
    }
}

/// 记下发出的邮件，不真发。给测试注入（[`crate::Config::mailer_override`]）。
#[derive(Default)]
pub struct RecordingMailer {
    sent: Mutex<Vec<OutgoingMail>>,
    fail_next: Mutex<u32>,
}

impl RecordingMailer {
    /// 目前为止「发出」的邮件。
    pub fn sent(&self) -> Vec<OutgoingMail> {
        self.sent.lock().expect("recording mailer").clone()
    }

    /// 让接下来的 `n` 次发信失败（模拟 SMTP 不可用）。
    pub fn fail_next(&self, n: u32) {
        *self.fail_next.lock().expect("recording mailer") = n;
    }
}

#[async_trait]
impl Mailer for RecordingMailer {
    async fn send(&self, mail: OutgoingMail) -> Result<(), MailError> {
        {
            let mut left = self.fail_next.lock().expect("recording mailer");
            if *left > 0 {
                *left -= 1;
                return Err(MailError::Smtp { status: Some("421".into()), message: "service not available".into() });
            }
        }
        self.sent.lock().expect("recording mailer").push(mail);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 邮箱规范化
// ---------------------------------------------------------------------------

/// 邮箱规范化：去首尾空白、整体小写，并检查是个合法的邮箱地址（≤ 254 字符、没有空白 / 控制字符）。
/// 不合法返回 `None`。登录、绑定、查找都用规范化后的形式。
pub fn normalize_email(raw: &str) -> Option<String> {
    let e = raw.trim().to_lowercase();
    if e.is_empty() || e.len() > 254 || e.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    e.parse::<lettre::Address>().ok()?;
    Some(e)
}

// ---------------------------------------------------------------------------
// 模板
// ---------------------------------------------------------------------------

/// 邮件语言。验证码邮件按请求的 `Accept-Language` 选；通知邮件中英双语。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

/// 从 `Accept-Language` 里选一个支持的语言（`zh*` 或 `en*`，按 q 值，同值取先出现的），没有就英文。
pub fn lang_from_accept_language(header: Option<&str>) -> Lang {
    let mut best: Option<(f32, Lang)> = None;
    for item in header.unwrap_or("").split(',') {
        let mut parts = item.split(';');
        let tag = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q=").map(str::to_string))
            .find_map(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        let primary = tag.split('-').next().unwrap_or("");
        let lang = match primary {
            "zh" => Lang::Zh,
            "en" => Lang::En,
            _ => continue,
        };
        if q > 0.0 && best.is_none_or(|(bq, _)| q > bq) {
            best = Some((q, lang));
        }
    }
    best.map_or(Lang::En, |(_, l)| l)
}

/// 验证码邮件的用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeKind {
    Login,
    Bind,
}

/// `123456` → `123 456`。
pub fn format_code(code: &str) -> String {
    if code.len() == 6 && code.is_ascii() {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

/// 页脚链接到的项目主页（中文邮件链到中文版）。
const SITE_URL: &str = "https://anycon.ai";
const SITE_URL_ZH: &str = "https://anycon.ai/zh";

/// 验证码邮件（主题，纯文本正文，HTML 正文）。`minutes` 是有效期。
///
/// 两份正文说的是同一件事：HTML 那份字号大、码单独一行放大，纯文本那份给不显示 HTML 的客户端。页脚都带项目主页的链接。
///
/// 英文纯文本的每一行都压在 78 个字符以内、全是 ASCII：这样 lettre 用 7bit 原样发出，码不会被 quoted-printable 的软换行
/// 从中间切开（e2e 里的假 SMTP 服务和各种邮件客户端的预览都更省事）。中文正文有非 ASCII 字符，lettre 会编码，收件方照常解码。
pub fn code_mail(lang: Lang, kind: CodeKind, code: &str, minutes: u64) -> (String, String, String) {
    let code = format_code(code);
    let (validity, ignore, tagline, site) = match lang {
        Lang::En => (
            format!("It is valid for {minutes} minutes."),
            "If you did not request it, ignore this email.",
            "Run coding agents on your own machine. Talk to them from anywhere.",
            SITE_URL,
        ),
        Lang::Zh => (
            format!("{minutes} 分钟内有效。"),
            "不是你本人操作就忽略这封邮件。",
            "coding agent 跑在你自己的机器上，从任何地方和它对话。",
            SITE_URL_ZH,
        ),
    };
    // （主题，纯文本里带码的那一段，HTML 里码上面的一句话）
    let (subject, text, lead) = match (lang, kind) {
        (Lang::En, CodeKind::Login) => (
            "Your Anycon sign-in code",
            format!("Your Anycon sign-in code is {code}.\n{validity}"),
            "Enter this code to sign in to Anycon:",
        ),
        (Lang::En, CodeKind::Bind) => (
            "Confirm your email address for Anycon",
            format!("Your code for confirming this email address in Anycon is {code}.\n{validity}"),
            "Enter this code in Anycon to confirm this email address:",
        ),
        (Lang::Zh, CodeKind::Login) => (
            "你的 Anycon 登录码",
            format!("你的 Anycon 登录码是 {code}，{validity}"),
            "输入下面的登录码，登录 Anycon：",
        ),
        (Lang::Zh, CodeKind::Bind) => (
            "确认你的 Anycon 邮箱",
            format!("你正在为 Anycon 账号绑定这个邮箱，验证码是 {code}，{validity}"),
            "输入下面的验证码，为你的 Anycon 账号绑定这个邮箱：",
        ),
    };
    let body = format!("{text}\n\n{ignore}\n\n--\nAnycon\n{tagline}\n{site}\n");
    let html = code_mail_html(&CodeMailHtml {
        lang,
        title: subject,
        lead,
        code: &code,
        validity: &validity,
        ignore,
        tagline,
        site,
    });
    (subject.into(), body, html)
}

/// [`code_mail_html`] 要填进版式里的文字。
struct CodeMailHtml<'a> {
    lang: Lang,
    title: &'a str,
    /// 码上面的一句话。
    lead: &'a str,
    code: &'a str,
    validity: &'a str,
    ignore: &'a str,
    /// 页脚的一句话介绍。
    tagline: &'a str,
    /// 页脚链接。
    site: &'a str,
}

/// `&`、`<`、`>`、`"` 换成实体。现在填进版式的都是固定文字和数字，转义是为了以后填进别的东西时不出事。
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// 验证码邮件的 HTML 版式：一张卡片（产品名、一句话、放大的码、有效期、「不是你就忽略」），下面是页脚。
///
/// 按邮件客户端认的写法来：表格布局、样式内联、字体写在每个单元格上（Outlook 不认 `div` 的内边距，也不把字体继承进表格）；
/// `<style>` 里只有深色模式的覆盖（不认的客户端显示浅色版）。
/// 不引用任何外部资源（图片、字体、样式表）：邮件客户端默认不加载远程图片，而且一加载就等于告诉发信方「这封信被打开了」。
/// 唯一的链接是页脚的项目主页，不带任何参数。
fn code_mail_html(t: &CodeMailHtml<'_>) -> String {
    const SANS: &str = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,\
        'PingFang SC','Microsoft YaHei',sans-serif";
    const MONO: &str = "ui-monospace,SFMono-Regular,Menlo,Consolas,'Liberation Mono',monospace";
    let lang = match t.lang {
        Lang::En => "en",
        Lang::Zh => "zh-CN",
    };
    let title = html_escape(t.title);
    let lead = html_escape(t.lead);
    let code = html_escape(t.code);
    let validity = html_escape(t.validity);
    let ignore = html_escape(t.ignore);
    let tagline = html_escape(t.tagline);
    let site = html_escape(t.site);
    format!(
        r#"<!DOCTYPE html>
<html lang="{lang}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<meta name="supported-color-schemes" content="light dark">
<title>{title}</title>
<style>
@media (prefers-color-scheme: dark) {{
  .page {{ background-color: #121314 !important; }}
  .card {{ background-color: #1e1f21 !important; }}
  .text {{ color: #e6e6ea !important; }}
  .muted {{ color: #9a9aa6 !important; }}
  .code {{ background-color: #2b2c30 !important; color: #ffffff !important; }}
  .link {{ color: #9d98ff !important; }}
}}
</style>
</head>
<body class="page" style="margin:0;padding:0;background-color:#f4f4f7;">
<table role="presentation" class="page" width="100%" cellpadding="0" cellspacing="0" border="0" style="background-color:#f4f4f7;">
<tr><td align="center" style="padding:32px 16px;">
<table role="presentation" width="520" cellpadding="0" cellspacing="0" border="0" style="width:100%;max-width:520px;">
<tr><td class="card" style="padding:36px 32px;border-radius:14px;background-color:#ffffff;">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0">
<tr><td class="text" style="font-family:{SANS};font-size:22px;line-height:28px;font-weight:700;color:#17171c;">Anycon</td></tr>
<tr><td class="text" style="padding-top:24px;font-family:{SANS};font-size:18px;line-height:28px;color:#17171c;">{lead}</td></tr>
<tr><td style="padding-top:20px;">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0">
<tr><td class="code" align="center" style="padding:22px 12px;border-radius:12px;background-color:#eef0ff;white-space:nowrap;font-family:{MONO};font-size:40px;line-height:48px;font-weight:700;letter-spacing:2px;color:#17171c;">{code}</td></tr>
</table>
</td></tr>
<tr><td class="text" style="padding-top:20px;font-family:{SANS};font-size:18px;line-height:28px;color:#17171c;">{validity}</td></tr>
<tr><td class="muted" style="padding-top:8px;font-family:{SANS};font-size:16px;line-height:24px;color:#565662;">{ignore}</td></tr>
</table>
</td></tr>
<tr><td class="muted" align="center" style="padding:24px 16px 0;font-family:{SANS};font-size:14px;line-height:22px;color:#565662;">
{tagline}<br>
<a class="link" href="{site}" style="font-weight:600;color:#4f46e5;text-decoration:none;">anycon.ai</a>
</td></tr>
</table>
</td></tr>
</table>
</body>
</html>
"#
    )
}

/// 可以发邮件通知的类别（docs/spec/hub.md § 6.2）。
pub fn is_email_category(c: anycon_protocol::NotifyCategory) -> bool {
    use anycon_protocol::NotifyCategory::*;
    matches!(c, PermissionRequested | Error | EnrollRequested | Security)
}

/// 去掉控制字符并截断：节点名是设备自报的，进邮件正文前先洗一遍。
fn clean_node_name(name: &str) -> String {
    let s: String = name.chars().filter(|c| !c.is_control()).take(100).collect();
    let s = s.trim();
    if s.is_empty() {
        "?".into()
    } else {
        s.to_string()
    }
}

/// 通知邮件（主题，正文）：固定模板，中英双语，只有节点名 + 类别 + 链接，不含任何会话内容。
/// 不是邮件类别的返回 `None`。
pub fn notification_mail(
    category: anycon_protocol::NotifyCategory,
    node_name: &str,
    link: &str,
) -> Option<(String, String)> {
    use anycon_protocol::NotifyCategory::*;
    let node = clean_node_name(node_name);
    let (en_subject, zh_subject, en, zh) = match category {
        PermissionRequested => (
            "A session is waiting for your approval",
            "有会话在等你审批",
            format!("A session on node \"{node}\" is waiting for your approval."),
            format!("节点「{node}」的一个会话在等你审批。"),
        ),
        Error => (
            "A session hit an error",
            "有会话出错了",
            format!("A session on node \"{node}\" hit an error."),
            format!("节点「{node}」的一个会话出错了。"),
        ),
        EnrollRequested => (
            "A device wants to join your node",
            "有设备请求接入",
            format!("A device is asking to join node \"{node}\"."),
            format!("节点「{node}」有设备请求接入。"),
        ),
        Security => (
            "Security notice for your node",
            "节点的安全提醒",
            format!(
                "Node \"{node}\" reported a security event:\na new device joined, repeated wrong passphrases or a lockout,\nor a settings change."
            ),
            format!("节点「{node}」报告了一个安全事件（有新设备接入、口令连续输错或被锁定，或设置被修改）。"),
        ),
        TurnFinished | SessionIdle => return None,
    };
    let subject = format!("[Anycon] {en_subject} / {zh_subject}");
    let body = format!(
        "{en}\n\nOpen: {link}\n\n{zh}\n打开：{link}\n\n--\nChange which notifications you get under Settings > Notifications.\n你可以在 Anycon 的「设置 > 通知」里调整要收到哪些通知。\n"
    );
    Some((subject, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::NotifyCategory;

    #[test]
    fn emails_are_trimmed_lowercased_and_validated() {
        assert_eq!(normalize_email("  Alice@Example.COM \n").as_deref(), Some("alice@example.com"));
        assert_eq!(normalize_email("a+tag@sub.example.org").as_deref(), Some("a+tag@sub.example.org"));
        for bad in [
            "",
            "   ",
            "alice",
            "alice@",
            "@example.com",
            "a b@example.com",
            "a@@example.com",
            "a@b c.com",
            "a\n@example.com",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad:?}");
        }
        let long = format!("{}@example.com", "a".repeat(250));
        assert_eq!(normalize_email(&long), None, "longer than 254 characters");
    }

    #[test]
    fn language_follows_accept_language() {
        assert_eq!(lang_from_accept_language(None), Lang::En);
        assert_eq!(lang_from_accept_language(Some("")), Lang::En);
        assert_eq!(lang_from_accept_language(Some("zh-CN,zh;q=0.9,en;q=0.8")), Lang::Zh);
        assert_eq!(lang_from_accept_language(Some("en-US,en;q=0.9,zh;q=0.5")), Lang::En);
        assert_eq!(lang_from_accept_language(Some("fr;q=0.9, zh-TW;q=0.4, en;q=0.2")), Lang::Zh);
        assert_eq!(lang_from_accept_language(Some("de, fr")), Lang::En, "unsupported languages fall back to English");
        assert_eq!(lang_from_accept_language(Some("zh;q=0, en;q=0.1")), Lang::En, "q=0 means not acceptable");
        assert_eq!(lang_from_accept_language(Some("*;q=0.5, zh")), Lang::Zh);
    }

    #[test]
    fn code_mails_carry_the_grouped_code_and_the_lifetime() {
        let (subject, body, _) = code_mail(Lang::En, CodeKind::Login, "123456", 10);
        assert!(subject.contains("sign-in code"));
        assert!(
            body.contains("123 456") && body.contains("10 minutes") && body.contains("ignore this email"),
            "{body}"
        );
        let (subject, body, _) = code_mail(Lang::Zh, CodeKind::Login, "123456", 10);
        assert!(subject.contains("登录码"));
        assert!(
            body.contains("123 456") && body.contains("10 分钟内有效") && body.contains("不是你本人操作就忽略这封邮件"),
            "{body}"
        );
        let (_, body, _) = code_mail(Lang::En, CodeKind::Bind, "000042", 10);
        assert!(body.contains("000 042") && body.contains("confirming this email"), "{body}");
        let (_, body, _) = code_mail(Lang::Zh, CodeKind::Bind, "000042", 10);
        assert!(body.contains("000 042") && body.contains("绑定这个邮箱"), "{body}");
        assert_eq!(format_code("12345"), "12345", "only six-digit codes are grouped");
    }

    #[test]
    fn english_code_mails_stay_plain_ascii_with_short_lines() {
        for kind in [CodeKind::Login, CodeKind::Bind] {
            let (subject, body, _) = code_mail(Lang::En, kind, "123456", 10);
            assert!(subject.is_ascii() && body.is_ascii());
            assert!(body.lines().all(|l| l.len() <= 76), "every line fits without a soft line break: {body:?}");
            assert!(body.lines().any(|l| l.contains("123 456")));
        }
    }

    #[test]
    fn code_mails_end_with_a_footer_that_links_to_the_project_site() {
        for kind in [CodeKind::Login, CodeKind::Bind] {
            let (_, body, html) = code_mail(Lang::En, kind, "123456", 10);
            let tagline = "Run coding agents on your own machine. Talk to them from anywhere.";
            assert!(body.ends_with(&format!("\n--\nAnycon\n{tagline}\nhttps://anycon.ai\n")), "{body}");
            assert!(html.contains(r#"href="https://anycon.ai""#) && html.contains(tagline), "{html}");
            let (_, body, html) = code_mail(Lang::Zh, kind, "123456", 10);
            assert!(body.ends_with("从任何地方和它对话。\nhttps://anycon.ai/zh\n"), "{body}");
            assert!(html.contains(r#"href="https://anycon.ai/zh""#) && html.contains("从任何地方和它对话。"), "{html}");
        }
    }

    #[test]
    fn the_html_code_mail_says_the_same_as_the_text_one_with_the_code_on_its_own() {
        let (subject, _, html) = code_mail(Lang::En, CodeKind::Login, "123456", 10);
        assert!(html.contains(r#"<html lang="en">"#) && html.contains(&format!("<title>{subject}</title>")), "{html}");
        assert!(html.contains(">123 456</td>"), "the code is an element of its own: {html}");
        assert!(html.contains("sign in to Anycon") && html.contains("valid for 10 minutes"), "{html}");
        assert!(html.contains("If you did not request it, ignore this email."), "{html}");
        let (_, _, html) = code_mail(Lang::En, CodeKind::Bind, "000042", 10);
        assert!(html.contains(">000 042</td>") && html.contains("confirm this email address"), "{html}");
        let (_, _, html) = code_mail(Lang::Zh, CodeKind::Login, "123456", 10);
        assert!(html.contains(r#"<html lang="zh-CN">"#) && html.contains(">123 456</td>"), "{html}");
        assert!(html.contains("登录 Anycon") && html.contains("10 分钟内有效"), "{html}");
        assert!(html.contains("不是你本人操作就忽略这封邮件"), "{html}");
        let (_, _, html) = code_mail(Lang::Zh, CodeKind::Bind, "000042", 10);
        assert!(html.contains(">000 042</td>") && html.contains("绑定这个邮箱"), "{html}");
    }

    #[test]
    fn the_html_code_mail_loads_nothing_remote_and_has_one_link() {
        for lang in [Lang::En, Lang::Zh] {
            let (_, _, html) = code_mail(lang, CodeKind::Login, "123456", 10);
            for remote in ["<img", "<link", "<script", "url(", "@import", "src=", "http://"] {
                assert!(!html.contains(remote), "{remote}: {html}");
            }
            assert_eq!(html.matches("href=").count(), 1, "{html}");
            assert_eq!(html.matches("https://").count(), 1, "{html}");
        }
    }

    #[test]
    fn text_put_into_the_html_mail_is_escaped() {
        assert_eq!(
            html_escape(r#"<a href="x">Tom & Jerry</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; Jerry&lt;/a&gt;"
        );
        let html = code_mail_html(&CodeMailHtml {
            lang: Lang::En,
            title: "<t>",
            lead: "a & b",
            code: "<script>",
            validity: "v",
            ignore: "i",
            tagline: "t",
            site: "https://example.test/?a=1&b=\"2\"",
        });
        assert!(!html.contains("<script>") && html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("<title>&lt;t&gt;</title>") && html.contains("a &amp; b"), "{html}");
        assert!(html.contains(r#"href="https://example.test/?a=1&amp;b=&quot;2&quot;""#), "{html}");
    }

    #[test]
    fn notification_mails_are_bilingual_fixed_templates() {
        for (cat, en, zh) in [
            (NotifyCategory::PermissionRequested, "waiting for your approval", "在等你审批"),
            (NotifyCategory::Error, "hit an error", "出错了"),
            (NotifyCategory::EnrollRequested, "asking to join", "请求接入"),
            (NotifyCategory::Security, "security event", "安全事件"),
        ] {
            let (subject, body) = notification_mail(cat, "my-laptop", "https://hub.example.com/n/abc").unwrap();
            assert!(subject.starts_with("[Anycon]"), "{subject}");
            assert!(body.contains(en) && body.contains(zh), "{cat:?}: {body}");
            assert!(body.contains("my-laptop") && body.contains("https://hub.example.com/n/abc"), "{body}");
            assert!(is_email_category(cat));
        }
        for cat in [NotifyCategory::TurnFinished, NotifyCategory::SessionIdle] {
            assert!(notification_mail(cat, "n", "l").is_none() && !is_email_category(cat));
        }
    }

    #[test]
    fn node_names_cannot_inject_lines_into_the_mail() {
        let (_, body) =
            notification_mail(NotifyCategory::Error, "evil\r\nBcc: x@y.z\n\tname", "https://h.example/n/a").unwrap();
        assert!(!body.contains("\r"), "{body:?}");
        assert!(!body.contains("evil\nBcc"), "{body:?}");
        let (_, body) = notification_mail(NotifyCategory::Error, "\u{7}\u{0}", "https://h.example/n/a").unwrap();
        assert!(body.contains("\"?\""), "an empty name becomes ?: {body}");
        let long = "x".repeat(500);
        let (_, body) = notification_mail(NotifyCategory::Error, &long, "https://h.example/n/a").unwrap();
        assert!(!body.contains(&"x".repeat(101)), "node names are clipped to 100 characters");
    }

    #[test]
    fn smtp_errors_are_redacted_before_they_reach_the_logs() {
        let e = MailError::Smtp { status: Some("550".into()), message: "<Alice@Example.com>: user unknown".into() };
        let shown = e.redacted("alice@example.com");
        assert!(!shown.to_lowercase().contains("alice@example.com"), "{shown}");
        assert!(shown.contains("550") && shown.contains("<redacted>") && shown.contains("user unknown"), "{shown}");
        assert_eq!(e.status(), Some("550"));
        let long = MailError::Smtp { status: None, message: "x".repeat(1000) };
        assert!(long.redacted("a@b.c").chars().count() <= 200);
        assert_eq!(MailError::BadAddress.status(), None);
    }

    #[test]
    fn debug_output_of_mail_and_stored_rows_has_no_addresses_codes_or_endpoints() {
        let mail = OutgoingMail {
            to: "alice@example.com".into(),
            subject: "s".into(),
            body: "code 123 456".into(),
            html: Some("<b>123 456</b>".into()),
        };
        let dump = format!("{mail:?}");
        assert!(!dump.contains("alice") && !dump.contains("123"), "{dump}");
        let row = crate::store::EmailCode {
            email: "alice@example.com".into(),
            purpose: crate::store::EmailPurpose::Login,
            account_id: None,
            code_hash: "deadbeef".into(),
            expires_at_ms: 1,
            attempts: 0,
            created_at_ms: 0,
        };
        let dump = format!("{row:?}");
        assert!(!dump.contains("alice") && !dump.contains("deadbeef"), "{dump}");
        let sub = crate::store::PushSubscriptionRow {
            id: "i".into(),
            device_id: "d".into(),
            device_name: "n".into(),
            endpoint: "https://push.example/SECRET-PATH".into(),
            created_at_ms: 0,
        };
        assert!(!format!("{sub:?}").contains("SECRET-PATH"));
    }

    #[tokio::test]
    async fn the_recording_mailer_keeps_what_it_was_asked_to_send() {
        let m = RecordingMailer::default();
        let mail = OutgoingMail { to: "a@b.c".into(), subject: "s".into(), body: "b".into(), html: None };
        m.send(mail.clone()).await.unwrap();
        m.fail_next(1);
        assert!(m.send(mail.clone()).await.is_err());
        m.send(mail.clone()).await.unwrap();
        assert_eq!(m.sent(), vec![mail.clone(), mail]);
    }
}
