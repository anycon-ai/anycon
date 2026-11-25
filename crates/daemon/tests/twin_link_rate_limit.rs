//! hub 限速 WebSocket 升级（HTTP 429 + `Retry-After`）时建立 Bot 连接（docs/spec/daemon.md § 27.2）：
//! 按 `Retry-After` 等一等再试；等不到就失败成 `busy`、`data.reason = "hub_rate_limited"`，A 上不留半截的状态。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_daemon::client::HubState;
use anycon_daemon::testutil::TestDaemon;
use anycon_protocol::*;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

/// 假 hub：前 `limited` 次升级回 429（`Retry-After: retry_after`），之后收下 WebSocket、对认证回 `AuthFail { protocol }`
/// （让建立 Bot 连接在「过了限速」之后以一个可识别的方式结束）。返回地址和升级请求计数。
async fn start_limiting_hub(limited: usize, retry_after: u64) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let n = count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if n < limited {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: {retry_after}\r\nContent-Length: 0\r\n\r\n"
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Binary(_) = msg {
                        let bytes = Envelope::control("", 1, &Control::AuthFail { reason: AuthFailReason::Protocol })
                            .and_then(|e| e.encode())
                            .unwrap();
                        let _ = ws.send(Message::Binary(bytes.into())).await;
                    }
                }
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

async fn link(td: &TestDaemon) -> (RpcError, Duration) {
    let mut cli = td.client("cli");
    let t = Instant::now();
    let e = cli
        .call::<Value>(td, methods::TWIN_LINK, json!({ "code": "AAAA-BBBB-CCCC-DDDD", "node_id": "some-node" }))
        .await
        .unwrap_err();
    (e, t.elapsed())
}

async fn assert_nothing_left(td: &TestDaemon, url: &str) {
    let mut cli = td.client("cli");
    let links: TwinLinksResult = cli.call(td, methods::TWIN_LINKS, json!({})).await.unwrap();
    assert!(links.links.is_empty());
    assert!(HubState::load(&td.paths.twin_dir(), url).nodes.is_empty());
}

#[tokio::test]
async fn linking_waits_for_the_hubs_retry_after_and_then_goes_on() {
    let (url, seen) = start_limiting_hub(2, 1).await;
    let td = TestDaemon::start_full(&[], true, Some(&url)).await;
    let (e, took) = link(&td).await;
    // 过了限速之后碰上的是「hub 不认 Bot 设备」：说明等过之后确实继续了
    assert_eq!(e.data.as_ref().and_then(|d| d.reason.as_deref()), Some("hub_unsupported"), "{e:?}");
    assert!(took >= Duration::from_millis(900), "waited for Retry-After: {took:?}");
    assert!(took < Duration::from_secs(20), "{took:?}");
    assert!(seen.load(Ordering::SeqCst) >= 3);
    assert_nothing_left(&td, &url).await;
}

#[tokio::test]
async fn a_hub_that_keeps_limiting_fails_the_link_as_hub_rate_limited() {
    // `Retry-After` 比愿意等的久：不等，直接说清楚原因和建议等多久
    let (url, _) = start_limiting_hub(usize::MAX, 120).await;
    let td = TestDaemon::start_full(&[], true, Some(&url)).await;
    let (e, took) = link(&td).await;
    assert_eq!(e.stable_code(), Some(ErrorCode::Busy), "{e:?}");
    assert_eq!(e.data.as_ref().and_then(|d| d.reason.as_deref()), Some("hub_rate_limited"), "{e:?}");
    assert!(e.message.contains("120 s"), "{}", e.message);
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert_nothing_left(&td, &url).await;
}
