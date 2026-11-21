//! 更新检查（ADR-57）接到 daemon 上：`node.info.update_available`、doctor 的 `update` 一项、状态文件和重启。
//! 只连本机回环地址上的假服务器；夹具缺省把检查关掉，这里显式打开。

use std::time::Duration;

use anycon_daemon::testutil::TestDaemon;
use anycon_daemon::update::{Endpoint, UpdateCheckOptions, STATE_FILE};
use anycon_protocol::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 每个连接回同一份 `releases.json`，数一共收到几个请求。
async fn fake_site(body: &'static str) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/releases.json", l.local_addr().unwrap());
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            h.fetch_add(1, Ordering::SeqCst);
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes()).await;
            let _ = s.shutdown().await;
        }
    });
    (url, hits)
}

fn opts(url: String) -> UpdateCheckOptions {
    UpdateCheckOptions {
        endpoint: Endpoint::Custom(url),
        first_delay: Some(Duration::ZERO),
        timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn node_info_and_doctor_report_a_newer_release_and_restart_does_not_recheck() {
    // `url` 指向别处、带控制字符：daemon 不用它，链接由版本号拼出来
    let (url, hits) = fake_site(
        r#"{"latest":"99.1.0","url":"https://evil.example/\u001b]8;;x","published_at":"2026-11-20T10:00:00Z","new_field":[1]}"#,
    )
    .await;
    let mut td = TestDaemon::start_with_update_check(opts(url)).await;
    let mut c = td.client("web");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let info = loop {
        let info: NodeInfo = c.call(&td, "node.info", json!({})).await.unwrap();
        if info.update_available.is_some() {
            break info;
        }
        assert!(std::time::Instant::now() < deadline, "node.info never carried the update");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let u = info.update_available.unwrap();
    assert_eq!(u.latest, "99.1.0");
    assert_eq!(u.url, "https://github.com/anycon-ai/anycon/releases/tag/v99.1.0");
    assert!(td.paths.home.join(STATE_FILE).exists());

    let doctor: NodeDoctorResult = c.call(&td, "node.doctor", json!({})).await.unwrap();
    let check = doctor.checks.iter().find(|c| c.id == "update").expect("an update check");
    assert!(check.ok);
    assert!(check.detail.contains("anycon 99.1.0 is available"), "{}", check.detail);
    assert!(!check.detail.contains("evil"), "{}", check.detail);

    // 重启：结果从状态文件读回，不再发请求（下一次在 24 小时之后）
    let before = hits.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(before, 1);
    td.restart().await;
    let mut c = td.client("web");
    let info: NodeInfo = c.call(&td, "node.info", json!({})).await.unwrap();
    assert_eq!(info.update_available.as_ref().map(|u| u.latest.as_str()), Some("99.1.0"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), before, "restart must not trigger a new request");

    // 关掉（热加载）：node.info 不再带它，doctor 说明是关着的
    anycon_daemon::config::set_value(&td.paths, "update.check", "false").unwrap();
    td.ctx.config.reload().unwrap();
    let info: NodeInfo = c.call(&td, "node.info", json!({})).await.unwrap();
    assert!(info.update_available.is_none());
    let doctor: NodeDoctorResult = c.call(&td, "node.doctor", json!({})).await.unwrap();
    let check = doctor.checks.iter().find(|c| c.id == "update").unwrap();
    assert!(check.detail.contains("check = false"), "{}", check.detail);
    td.shutdown().await;
}

#[tokio::test]
async fn an_older_or_equal_release_is_not_reported() {
    let (url, _hits) = fake_site(r#"{"latest":"0.0.1"}"#).await;
    let td = TestDaemon::start_with_update_check(opts(url)).await;
    let state = td.paths.home.join(STATE_FILE);
    let t = std::time::Instant::now();
    while !state.exists() {
        assert!(t.elapsed() < Duration::from_secs(10), "no check happened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut c = td.client("web");
    let info: NodeInfo = c.call(&td, "node.info", json!({})).await.unwrap();
    assert!(info.update_available.is_none());
    // 序列化里也没有这个字段（旧客户端看到的和以前一样）
    let raw: serde_json::Value = c.call(&td, "node.info", json!({})).await.unwrap();
    assert!(raw.get("update_available").is_none(), "{raw}");
    td.shutdown().await;
}

#[tokio::test]
async fn fixtures_have_the_check_off() {
    let td = TestDaemon::start().await;
    assert_eq!(td.ctx.updates.off_reason(), Some(anycon_daemon::update::Off::Embedder));
    td.shutdown().await;
}
