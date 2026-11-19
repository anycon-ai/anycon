//! 清单的标题和正文不进日志（docs/spec/daemon.md § 21.4、§ 17）。单独一个测试进程：用全局 tracing 订阅者收集输出，
//! 不会和别的测试线程互相干扰。

use anycon_daemon::testutil::TestDaemon;
use anycon_protocol::*;
use serde_json::{json, Value};

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn add_t(td: &TestDaemon, c: &mut anycon_daemon::testutil::TestClient, p: Value) -> Result<Todo, RpcError> {
    c.call(td, methods::TODO_ADD, p).await
}
async fn update_t(td: &TestDaemon, c: &mut anycon_daemon::testutil::TestClient, p: Value) -> Result<Todo, RpcError> {
    c.call(td, methods::TODO_UPDATE, p).await
}
async fn list_t(
    td: &TestDaemon,
    c: &mut anycon_daemon::testutil::TestClient,
    p: Value,
) -> Result<TodoListResult, RpcError> {
    c.call(td, methods::TODO_LIST, p).await
}

/// 测试里收集 tracing 输出。
#[derive(Clone, Default)]
struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

#[tokio::test]
async fn titles_and_bodies_never_reach_the_logs_including_on_error_paths() {
    let td = TestDaemon::start().await;
    let mut c = td.local_cli_client();
    let logs = LogBuf::default();
    let sub = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    tracing::subscriber::set_global_default(sub).unwrap();

    let t = add_t(
        &td,
        &mut c,
        json!({ "id": id(), "title": "LOGSECRET-title", "body": "LOGSECRET-body", "project": "LOGSECRET-proj" }),
    )
    .await
    .unwrap();
    // 错误路径：标题超长（里面带着标记）、不合法的 project、不存在的项、不规范的 id、类型不对的参数
    let _ = add_t(&td, &mut c, json!({ "id": id(), "title": format!("LOGSECRET-{}", "x".repeat(300)) })).await;
    let _ = add_t(&td, &mut c, json!({ "id": id(), "title": "LOGSECRET-ok", "project": "LOGSECRET\nnl" })).await;
    let _ = add_t(&td, &mut c, json!({ "id": id(), "title": "LOGSECRET-ok", "body": "LOGSECRET".repeat(3000) })).await;
    let _ = add_t(&td, &mut c, json!({ "id": "LOGSECRET", "title": "LOGSECRET-ok" })).await;
    let _ = add_t(&td, &mut c, json!({ "id": id(), "title": 7, "body": "LOGSECRET" })).await;
    update_t(
        &td,
        &mut c,
        json!({ "id": t.id, "title": "LOGSECRET-new", "body": "LOGSECRET-newbody", "state": "done" }),
    )
    .await
    .unwrap();
    let _ = update_t(&td, &mut c, json!({ "id": id(), "title": "LOGSECRET-x" })).await;
    let _ = update_t(&td, &mut c, json!({ "id": t.id, "title": "" , "body": "LOGSECRET"})).await;
    let _ = list_t(&td, &mut c, json!({ "project": "LOGSECRET-proj", "include_closed": true })).await;
    let _: Value = c.call(&td, methods::TODO_DELETE, json!({ "id": t.id })).await.unwrap();
    let _ = c.call::<Value>(&td, methods::TODO_DELETE, json!({ "id": t.id })).await;

    let out = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("rpc handled"), "the capture works: {out:?}");
    assert!(!out.contains("LOGSECRET"), "content reached the logs:\n{out}");
    td.shutdown().await;
}
