//! `POST /capture`（PWA 分享目标，docs/spec/hub.md § 2.1）：没有这个处理的 service worker 接管时分享的那一次 POST 到了 hub。
//! hub 回 `303 See Other` → `/capture`，请求体丢弃（不解析、不存、不进日志；最多读 1 MiB，读完才回答，所以浏览器发一大段文字也看得到 303）。
//! 这里发带标记的请求体：几 KiB 和 512 KiB 的 multipart / 纯文本在任何平台上都得到 303；超过 1 MiB 的上限，客户端可能看到 303，
//! 也可能看到连接中止（接受），但服务器之后照常工作；标记没有出现在 hub 的 trace 级日志和数据目录里的任何文件里。

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

const MARKER: &str = "SHARED-TEXT-MARKER-9f3a1c77";

fn files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

fn multipart(boundary: &str, fields: &[(&str, String)]) -> String {
    let mut body = String::new();
    for (name, value) in fields {
        body.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    body
}

async fn post(hub: &TestHub, content_type: &str, body: String) -> Result<reqwest::Response, reqwest::Error> {
    no_redirect_client().post(hub.url("/capture")).header("content-type", content_type).body(body).send().await
}

#[tokio::test]
async fn post_capture_redirects_and_discards_the_body_without_parsing_storing_or_logging_it() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("trace"))
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish(),
    );
    let hub = TestHub::start().await;
    let client = no_redirect_client();
    let boundary = "----anycon-test-boundary";
    let ct = format!("multipart/form-data; boundary={boundary}");
    // 手写 multipart（和浏览器的分享表单同一个形状），没有登录、没有 X-Anycon 头：也是 303，不是 401 / 403
    let small = multipart(
        boundary,
        &[
            ("title", format!("title {MARKER}")),
            ("text", format!("text {MARKER}")),
            ("url", format!("https://example.test/{MARKER}")),
        ],
    );
    let res = post(&hub, &ct, small).await.unwrap();
    assert_eq!(res.status(), 303);
    assert_eq!(res.headers()["location"], "/capture");

    // 512 KiB（超过 API 的 64 KiB 上限）：multipart 和纯文本都是 303，不是 413，也不是连接中止
    let medium = multipart(boundary, &[("text", format!("{MARKER}{}", "x".repeat(512 * 1024)))]);
    let res = post(&hub, &ct, medium).await.unwrap();
    assert_eq!(res.status(), 303);
    let res = post(&hub, "text/plain", format!("{MARKER}{}", "y".repeat(512 * 1024))).await.unwrap();
    assert_eq!(res.status(), 303);

    // 一个不是 multipart 的、不带内容的 POST 也一样
    let res = client.post(hub.url("/capture")).send().await.unwrap();
    assert_eq!(res.status(), 303);

    // 重定向的目标是 SPA 的页面，GET 能拿到（回退到 index.html）
    let page = client.get(hub.url("/capture")).send().await.unwrap();
    assert_eq!(page.status(), 200);

    // 日志里没有标记（也没有任何 query / body）
    let log = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
    assert!(log.contains("/capture"), "the capture of the log works: the request itself is logged by route");
    assert!(!log.contains(MARKER), "the shared text must not be logged");
    assert!(!log.contains("example.test"), "the shared url must not be logged");

    // 数据目录的任何文件里都没有
    let mut files = Vec::new();
    files_under(hub.dir.path(), &mut files);
    for f in files {
        let bytes = std::fs::read(&f).unwrap_or_default();
        assert!(!String::from_utf8_lossy(&bytes).contains(MARKER), "{} must not contain the shared text", f.display());
    }
}

#[tokio::test]
async fn a_body_above_the_drain_cap_gets_a_303_or_an_aborted_connection_and_the_server_keeps_working() {
    let hub = TestHub::start().await;
    // 5 MiB：超过丢弃的上限（1 MiB），hub 读到上限就回答；客户端可能收到 303，也可能在还在发的时候看到连接中止（接受）
    let big = format!("{MARKER}{}", "x".repeat(5 * 1024 * 1024));
    match post(&hub, "text/plain", big).await {
        Ok(res) => assert_eq!(res.status(), 303),
        Err(e) => assert!(
            e.is_request()
                || e.is_connect()
                || e.is_body()
                || e.is_timeout()
                || e.to_string().contains("error sending request"),
            "unexpected error kind: {e}"
        ),
    }
    // 服务器之后照常工作：新连接上的 POST 和 GET
    let res = post(&hub, "text/plain", "short".to_string()).await.unwrap();
    assert_eq!(res.status(), 303);
    let page = no_redirect_client().get(hub.url("/capture")).send().await.unwrap();
    assert_eq!(page.status(), 200);
}
