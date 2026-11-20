//! `POST /capture`（PWA 分享目标，docs/spec/hub.md § 2.1）：没有这个处理的 service worker 接管时分享的那一次 POST 到了 hub。
//! hub 只回 `303 See Other` → `/capture`：不读请求体、不存、不进日志。这里发一个带标记的 multipart 请求体，
//! 断言重定向，并且标记没有出现在 hub 的 trace 级日志和数据目录里的任何文件里。

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

#[tokio::test]
async fn post_capture_redirects_without_reading_storing_or_logging_the_body() {
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
    // 手写 multipart（和浏览器的分享表单同一个形状）
    let boundary = "----anycon-test-boundary";
    let mut body = String::new();
    for (name, value) in [
        ("title", format!("title {MARKER}")),
        ("text", format!("text {MARKER}")),
        ("url", format!("https://example.test/{MARKER}")),
    ] {
        body.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    // 没有登录、没有 X-Anycon 头：也是 303，不是 401 / 403
    let res = client
        .post(hub.url("/capture"))
        .header("content-type", format!("multipart/form-data; boundary={boundary}"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 303);
    assert_eq!(res.headers()["location"], "/capture");

    // 很大的请求体（超过 API 的 64 KiB 上限和 axum 的默认上限）：同样是 303，不是 413
    let big = format!("{MARKER}{}", "x".repeat(5 * 1024 * 1024));
    let res = client.post(hub.url("/capture")).header("content-type", "text/plain").body(big).send().await.unwrap();
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
