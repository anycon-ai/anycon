//! `anycon-hub` 二进制。
//!
//! - `anycon-hub`（或 `serve`）：按 `ANYCON_*` 环境变量启动；
//! - `anycon-hub healthcheck`：对本机 `/healthz` 发一个 HTTP GET，200 退出 0，否则退出 1
//!   （给 distroless 镜像的 `HEALTHCHECK` 用，镜像里没有 curl）；
//! - `anycon-hub version`。

use std::process::ExitCode;
use std::time::Duration;

use anycon_hub::{init_tracing, Config, VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `anycon-hub help` 里的环境变量说明（只列和部署安全相关的几个，全部变量见 docs/deploy.md）。
const ENV_HELP: &str = "ENVIRONMENT (the ones that matter for a safe deployment):
  ANYCON_PUBLIC_URL        required. Public http(s) origin without a path, e.g. https://hub.example.com.
                           When it is https the session cookie is always `Secure`.
  ANYCON_TRUSTED_PROXIES   comma-separated CIDRs/IPs of reverse proxies whose X-Forwarded-For and
                           X-Forwarded-Proto are believed. Default: 172.16.0.0/12 (the Docker default
                           bridge range). Outside Docker, set it explicitly: a client inside that range
                           can otherwise spoof its IP (rate limits) and the protocol. Empty = trust nobody;
                           then, behind a proxy, all clients share the proxy's rate-limit buckets.
                           IPv6 clients are rate-limited per /64.
  ANYCON_MAX_ENVELOPE_BYTES  largest relayed envelope, header included (default 262144, minimum 205824 = 201 KiB).
                           The minimum is one full 200-KiB fragment envelope: clients split every message into
                           fragments of that size, so a smaller limit would drop every full fragment and the hub
                           refuses to start. Bigger envelopes are dropped and the sender gets a
                           `dropped { too_large }` notice; frames over twice the limit close the connection.
  ANYCON_RATE_CLIENT_BYTES_PER_MIN  inbound byte budget per client connection (web / cli / sdk, and connections
                           that have no device token yet), default 10485760 (10 MiB). Minimum 205824.
  ANYCON_RATE_DAEMON_BYTES_PER_MIN  inbound byte budget per daemon connection, default 104857600 (100 MiB): a
                           daemon is the hub for all of its clients. Minimum 205824.
                           Envelopes over a budget (or over 500 envelopes/second) are never dropped: the hub hands
                           them back with `backpressure`, tells the sender how long to wait with `rate_limited`
                           (at most one per second), and the sender resends them in order. Only a connection that
                           is handed back for 10+ seconds without getting anything through is closed (1008).
  ANYCON_SMTP_URL          optional. Enables email-code login, email binding and email notifications.
                           smtps://user:pass@host:465 (implicit TLS), smtp://user:pass@host:587 (STARTTLS, must
                           succeed) or smtp+plain://host:25 (no encryption: localhost / private networks only.
                           An IP address outside those ranges makes the hub refuse to start; a host name is
                           resolved before every mail and every address must be local/private, else that mail
                           fails and is logged). Percent-encode special characters in user/pass.
  ANYCON_EMAIL_FROM        required when ANYCON_SMTP_URL is set: `Anycon <noreply@example.com>` or a bare address.
  ANYCON_VAPID_SUBJECT     optional. Contact for the push services (`mailto:you@example.com` or an https URL);
                           default: ANYCON_PUBLIC_URL when it is https, else mailto: + ANYCON_EMAIL_FROM.
                           The Web Push key pair is generated on first start into <data>/vapid.json: back it up.
  ANYCON_ADMIN_TOKEN       fixed admin login token (default: generated on first start, see <data>/admin-token).
  ANYCON_DATA_DIR / ANYCON_LISTEN / ANYCON_LOG   data directory (/data), bind address (0.0.0.0:8080), log filter (info).
";

#[tokio::main]
async fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        None | Some("serve") => serve().await,
        Some("healthcheck") => healthcheck().await,
        Some("version" | "--version" | "-V") => {
            println!("anycon-hub {VERSION}");
            ExitCode::SUCCESS
        }
        Some("help" | "--help" | "-h") => {
            println!(
                "anycon-hub {VERSION}\n\nUSAGE:\n  anycon-hub [serve]     start the hub (configured with ANYCON_* environment variables)\n  anycon-hub healthcheck probe http://127.0.0.1:<port>/healthz, exit 0 if healthy\n  anycon-hub version\n\n{ENV_HELP}\nSee docs/deploy.md for the environment variables."
            );
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("anycon-hub: unknown command {other:?} (try `anycon-hub help`)");
            ExitCode::from(2)
        }
    }
}

async fn serve() -> ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("anycon-hub: configuration error: {e}");
            return ExitCode::from(2);
        }
    };
    let log =
        std::env::var("ANYCON_LOG").ok().filter(|s| !s.trim().is_empty()).or_else(|| std::env::var("RUST_LOG").ok());
    init_tracing(log.as_deref().unwrap_or(&config.log));

    let public_url = config.public_url_str();
    let mode = config.mode.as_str();
    let mut handle = match anycon_hub::run(config).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("anycon-hub: failed to start: {e}");
            return ExitCode::from(1);
        }
    };

    // 启动横幅（deploy.md § 2）。admin token 明文只在首次生成时打印这一次，直接写 stdout，不进结构化日志。
    println!("anycon-hub {VERSION} listening on {}", handle.addr);
    println!("mode: {mode}");
    if let Some(token) = &handle.generated_admin_token {
        println!("first run — admin token (also saved to {}):", handle.admin_token_file.display());
        println!("  {token}");
        println!("open {public_url} and paste it to log in");
    } else {
        println!("admin token: see ANYCON_ADMIN_TOKEN or {}", handle.admin_token_file.display());
    }

    tokio::select! {
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal received, closing connections");
            handle.shutdown().await;
        }
        _ = handle.wait() => {
            tracing::error!("http server exited unexpectedly");
            return ExitCode::from(1);
        }
    }
    tracing::info!("bye");
    ExitCode::SUCCESS
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// 本机健康检查：不依赖 curl，也不引入 HTTP 客户端库。
async fn healthcheck() -> ExitCode {
    let port = std::env::var("ANYCON_LISTEN")
        .ok()
        .and_then(|s| s.trim().parse::<std::net::SocketAddr>().ok())
        .map_or(8080, |a| a.port());
    let probe = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        s.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n").await?;
        let mut buf = Vec::new();
        s.take(4096).read_to_end(&mut buf).await?;
        Ok::<_, std::io::Error>(buf)
    };
    match tokio::time::timeout(Duration::from_secs(3), probe).await {
        Ok(Ok(buf)) if buf.starts_with(b"HTTP/1.1 200") || buf.starts_with(b"HTTP/1.0 200") => ExitCode::SUCCESS,
        Ok(Ok(_)) => {
            eprintln!("healthcheck: unexpected response");
            ExitCode::from(1)
        }
        Ok(Err(e)) => {
            eprintln!("healthcheck: {e}");
            ExitCode::from(1)
        }
        Err(_) => {
            eprintln!("healthcheck: timeout");
            ExitCode::from(1)
        }
    }
}
