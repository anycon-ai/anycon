//! 在不认识 `twin` 设备种类的旧 hub 上建立 Twin 连接（docs/spec/daemon.md § 27.2）：假 hub 对每次认证都回 `protocol` 拒绝，
//! `twin.link` 要回 `unsupported`、`data.reason = "hub_unsupported"`，A 上不留半截的状态。

use anycon_daemon::client::HubState;
use anycon_daemon::testutil::TestDaemon;
use anycon_protocol::*;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

/// 假 hub：收下 WebSocket、读到认证就回 `AuthFail { protocol }`。返回它的地址。
async fn start_old_hub() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
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
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn linking_on_a_hub_that_rejects_twin_devices_says_the_hub_is_too_old_and_leaves_nothing_behind() {
    let url = start_old_hub().await;
    let td = TestDaemon::start_full(&[], true, Some(&url)).await;
    let mut cli = td.client("cli");
    let e = cli
        .call::<Value>(&td, methods::TWIN_LINK, json!({ "code": "AAAA-BBBB-CCCC-DDDD", "node_id": "some-node" }))
        .await
        .unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Unsupported), "{e:?}");
    assert_eq!(e.data.as_ref().and_then(|d| d.reason.as_deref()), Some("hub_unsupported"), "{e:?}");
    assert!(e.message.contains("update the hub"), "{}", e.message);

    // 没有留下半截的状态：没有连接记录，没有存下的节点
    let links: TwinLinksResult = cli.call(&td, methods::TWIN_LINKS, json!({})).await.unwrap();
    assert!(links.links.is_empty());
    assert!(HubState::load(&td.paths.twin_dir(), &url).nodes.is_empty());
}
