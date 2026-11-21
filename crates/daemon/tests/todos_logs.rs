//! 清单的标题和正文、派发的第一条消息和 agent 写的结果（「经过」里的 `text`）不进日志（docs/spec/daemon.md § 21.4、§ 22.4、§ 17）。单独一个测试进程：用全局 tracing 订阅者收集输出，
//! 不会和别的测试线程互相干扰。

use std::time::{Duration, Instant};

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
    let td = TestDaemon::start_git(&[]).await;
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

    // 派发和「经过」（§ 22）：第一条消息、agent 的复述（result）、各条错误路径
    // git 项目：派发的会话在 worktree 里，分支名和路径也要不带标题的字词
    td.new_repo("proj");
    let dt: Todo = c
        .call(
            &td,
            methods::TODO_ADD,
            json!({ "id": uuid::Uuid::new_v4().to_string(), "title": "LOGSECRET-title", "body": "LOGSECRET-body", "project": "proj" }),
        )
        .await
        .unwrap();
    // 成功的派发：第一条消息里有标题和正文，agent 复述它们（result 里也有）
    let dr: TodoDispatchResult =
        c.call(&td, methods::TODO_DISPATCH, json!({ "id": dt.id, "agent": "fake", "no_push": false })).await.unwrap();
    let t0 = Instant::now();
    loop {
        let h: TodoHistoryResult = c.call(&td, methods::TODO_HISTORY, json!({ "id": dt.id })).await.unwrap();
        if h.events.iter().any(|e| matches!(&e.data, TodoEventData::Result { text, .. } if text.contains("LOGSECRET")))
        {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "the result never arrived");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // 第二次派发（带着第一次的结果）、各种错误路径：不支持 no_push、项目不存在、设置不合法、不存在的项、不规范的 id、已关闭
    let _: Todo = c.call(&td, methods::TODO_UPDATE, json!({ "id": dt.id, "state": "review" })).await.unwrap();
    let _: TodoDispatchResult =
        c.call(&td, methods::TODO_DISPATCH, json!({ "id": dt.id, "agent": "fake", "no_push": false })).await.unwrap();
    let _ = c.call::<Value>(&td, methods::TODO_DISPATCH, json!({ "id": dt.id, "agent": "fake" })).await;
    let _ = c
        .call::<Value>(
            &td,
            methods::TODO_DISPATCH,
            json!({ "id": dt.id, "agent": "fake", "project": "LOGSECRET-proj", "no_push": false }),
        )
        .await;
    let _ = c
        .call::<Value>(
            &td,
            methods::TODO_DISPATCH,
            json!({ "id": dt.id, "agent": "fake", "no_push": false, "settings": { "model": "LOGSECRET-model" } }),
        )
        .await;
    let _ = c
        .call::<Value>(&td, methods::TODO_DISPATCH, json!({ "id": uuid::Uuid::new_v4().to_string(), "agent": "fake" }))
        .await;
    let _ = c.call::<Value>(&td, methods::TODO_DISPATCH, json!({ "id": "LOGSECRET", "agent": "fake" })).await;
    let _ = c.call::<Value>(&td, methods::TODO_DISPATCH, json!({ "id": 7 })).await;
    let _ = c.call::<Value>(&td, methods::TODO_HISTORY, json!({ "id": "LOGSECRET" })).await;
    let _ = c.call::<Value>(&td, methods::TODO_HISTORY, json!({ "id": dt.id, "before_seq": "LOGSECRET" })).await;
    let _: Todo = c.call(&td, methods::TODO_UPDATE, json!({ "id": dt.id, "state": "dropped" })).await.unwrap();
    let _ =
        c.call::<Value>(&td, methods::TODO_DISPATCH, json!({ "id": dt.id, "agent": "fake", "no_push": false })).await;
    let hh: TodoHistoryResult = c.call(&td, methods::TODO_HISTORY, json!({ "id": dt.id, "limit": 200 })).await.unwrap();
    assert!(hh.events.len() >= 3);
    let _: Empty = c.call(&td, methods::SESSION_DELETE, json!({ "session_id": dr.session.id })).await.unwrap();
    let _: Empty = c.call(&td, methods::TODO_DELETE, json!({ "id": dt.id })).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let out = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("rpc handled"), "the capture works: {out:?}");
    // 不区分大小写：分支名的 slug 是小写的，大小写敏感的比较抓不到它
    assert!(!out.to_lowercase().contains("logsecret"), "content reached the logs:\n{out}");
    let audit = serde_json::to_string(&td.ctx.audit.query(None, None, 1000).await).unwrap();
    assert!(audit.contains("worktree.create") && audit.contains("todo.dispatch"), "{audit}");
    // 项目名本来就在审计里（`todo.add` 的 `project`），所以按内容（标题、正文、结果）的写法找，不找 `logsecret` 本身
    let audit = audit.to_lowercase();
    for needle in
        ["logsecret-title", "logsecret-body", "logsecret-new", "logsecret-ok", "logsecret-x", "logsecret-model"]
    {
        assert!(!audit.contains(needle), "{needle} reached the audit log:\n{audit}");
    }
    td.shutdown().await;
}
