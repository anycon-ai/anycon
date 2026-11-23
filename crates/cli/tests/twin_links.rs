//! `anycon twin link` / `links` / `unlink`（A 这一侧，docs/spec/cli.md § 3.10）：真 hub（进程内）和两个 daemon，
//! CLI 是真的二进制，各自用自己的 home 连自己节点的本机 socket。

mod common;

use std::time::Duration;

use anycon_crypto::Keypair;
use anycon_daemon::testutil::{eventually, FakeWeb, TestDaemon};
use anycon_protocol::*;
use common::*;
use serde_json::json;

const T: Duration = Duration::from_secs(30);

struct TestHub {
    handle: anycon_hub::Handle,
    _dir: tempfile::TempDir,
}

impl TestHub {
    async fn start() -> TestHub {
        let dir = tempfile::tempdir().unwrap();
        let mut c = anycon_hub::Config::new("http://127.0.0.1", dir.path()).unwrap();
        c.listen = "127.0.0.1:0".parse().unwrap();
        let handle = anycon_hub::run(c).await.expect("hub starts");
        TestHub { handle, _dir: dir }
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.handle.addr.port())
    }
    async fn admin_cookie(&self) -> String {
        let res = reqwest::Client::new()
            .post(format!("{}/auth/token", self.url()))
            .header("x-anycon", "1")
            .json(&json!({ "token": self.handle.generated_admin_token.clone().unwrap() }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string()
    }
}

/// 起一个连着 hub、账号已经建好的 daemon。
async fn node(hub: &TestHub) -> TestDaemon {
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    assert!(eventually(T, || async { td.ctx.hub_state.get().connected }).await);
    let mut local = td.client("setup");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web =
        FakeWeb::connect(&hub.url(), None, Some(&hub.admin_cookie().await), "Setup", Keypair::generate()).await;
    web.pair(&begin.code).await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    td
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_links_and_unlink_drive_the_twin_connections_of_this_node() {
    let hub = TestHub::start().await;
    let a = node(&hub).await;
    let b = node(&hub).await;
    let b_id = b.ctx.identity.device_id().unwrap();

    // B 上打印短码；A 上用它建立连接（节点 id 不对：失败，A 上什么也没留下）
    let code = anycon(&b.paths.home, &["twin", "link-code", "--json"], None, &[]).await;
    assert_eq!(code.code, 0, "{}", code.stderr);
    let code = code.json()["code"].as_str().unwrap().to_string();
    let wrong = anycon(&a.paths.home, &["twin", "link", &code, "--peer", "not-b"], None, &[]).await;
    assert_ne!(wrong.code, 0);
    assert!(wrong.stderr.contains("node_mismatch"), "{}", wrong.stderr);
    let none = anycon(&a.paths.home, &["twin", "links"], None, &[]).await;
    assert_eq!(none.code, 0, "{}", none.stderr);
    assert!(none.stdout.contains("no linked nodes"), "{}", none.stdout);

    let code = anycon(&b.paths.home, &["twin", "link-code", "--json"], None, &[]).await.json()["code"]
        .as_str()
        .unwrap()
        .to_string();
    let linked = anycon(&a.paths.home, &["twin", "link", &code, "--peer", &b_id], None, &[]).await;
    assert_eq!(linked.code, 0, "{}", linked.stderr);
    assert!(
        linked.stdout.contains("Linked") && linked.stdout.contains("anycon twin set --requester"),
        "{}",
        linked.stdout
    );

    // `links`：名称、在线、B 开放了什么；`--json` 是 `{ links }`
    let mut online = false;
    for _ in 0..100 {
        let r = anycon(&a.paths.home, &["twin", "links", "--json"], None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        let v = r.json();
        assert_eq!(v["links"][0]["node_id"], json!(b_id));
        if v["links"][0]["online"] == json!(true) {
            online = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(online, "the link comes online");
    let text = anycon(&a.paths.home, &["twin", "links"], None, &[]).await;
    assert!(
        text.stdout.contains(&b_id[..8]) && text.stdout.contains("online") && text.stdout.contains("none opened"),
        "{}",
        text.stdout
    );

    // `unlink`：id 前缀；对不上的是用法错误；B 上的设备记录还在
    let no = anycon(&a.paths.home, &["twin", "unlink", "nobody"], None, &[]).await;
    assert_eq!(no.code, 2, "{}", no.stderr);
    let r = anycon(&a.paths.home, &["twin", "unlink", &b_id[..8]], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("revoke it there"), "{}", r.stdout);
    let after = anycon(&a.paths.home, &["twin", "links", "--json"], None, &[]).await;
    assert_eq!(after.json()["links"], json!([]));
    let devices: NodeDevicesResult = b.client("probe").call(&b, methods::NODE_DEVICES, json!({})).await.unwrap();
    assert!(devices.devices.iter().any(|d| d.kind == DeviceKind::Twin));

    // 没连 hub 的节点：`link` 是「离线」
    let lone = TestDaemon::start_with(&[], true).await;
    let offline = anycon(&lone.paths.home, &["twin", "link", "AAAA-BBBB-CCCC-DDDD", "--peer", &b_id], None, &[]).await;
    assert_eq!(offline.code, 3, "{}", offline.stderr);
    lone.shutdown().await;
    a.shutdown().await;
    b.shutdown().await;
}
