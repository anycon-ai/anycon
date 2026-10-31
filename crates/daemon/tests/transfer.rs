//! `anycon export` / `anycon import`（docs/spec/daemon.md § 20）：`local.export` / `local.import` 的往返、筛选、重复导入、
//! 恶意压缩包、本机专用、进度和取消。
//!
//! 夹具：一台 daemon 用假 adapter 真的跑出会话（含附件）再导出；恶意压缩包是把这份合法的导出拆开、改一处、重新打包得到的。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anycon_daemon::blobs::CHUNK_SIZE;
use anycon_daemon::clients::{Caller, ClientHandle};
use anycon_daemon::store::{SessionRow, WorktreeRow};
use anycon_daemon::testutil::{assert_mode, png_fixture, sha256_hex, TestClient, TestDaemon};
use anycon_daemon::transfer::{ExportResult, ImportResult, Limits, Progress, PROGRESS_METHOD};
use anycon_daemon::{local_methods, util};
use anycon_protocol::*;
use base64::Engine as _;
use serde_json::{json, Value};
use sqlx::Row;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const T: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- 夹具

fn code(e: &RpcError) -> ErrorCode {
    e.stable_code().expect("a stable error code")
}

fn reason(e: &RpcError) -> Option<&str> {
    e.data.as_ref().and_then(|d| d.reason.as_deref())
}

async fn upload(td: &TestDaemon, c: &mut TestClient, name: &str, bytes: &[u8]) -> AttachmentRef {
    let begin: BlobPutBeginResult = c
        .call(
            td,
            methods::BLOB_PUT_BEGIN,
            json!({ "name": name, "mime": "image/png", "size": bytes.len(), "sha256": sha256_hex(bytes) }),
        )
        .await
        .unwrap();
    for (i, part) in bytes.chunks(CHUNK_SIZE as usize).enumerate() {
        let data = base64::engine::general_purpose::STANDARD.encode(part);
        let _: Empty = c
            .call(td, methods::BLOB_PUT_CHUNK, json!({ "blob_id": begin.blob_id, "index": i, "data": data }))
            .await
            .unwrap();
    }
    c.call(td, methods::BLOB_PUT_END, json!({ "blob_id": begin.blob_id })).await.unwrap()
}

/// 创建带第一条消息的会话，等到它回到 idle。
async fn make_session(
    td: &TestDaemon,
    c: &mut TestClient,
    project: Option<&str>,
    prompt: &str,
    atts: &[&AttachmentRef],
) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let s: SessionSummary = c
        .call(
            td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": project, "prompt": prompt, "message_id": format!("m-{n}"), "attachments": atts }),
        )
        .await
        .unwrap();
    wait_idle(td, c, &s.id).await;
    s.id
}

async fn wait_idle(td: &TestDaemon, c: &mut TestClient, sid: &str) {
    let deadline = std::time::Instant::now() + T;
    loop {
        let s: SessionSummary = c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
        if s.status == SessionStatus::Idle && s.last_seq > 0 {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "the session reaches idle");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn history(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<SessionEvent> {
    let mut all = Vec::new();
    loop {
        let r: SessionHistoryResult = c
            .call(
                td,
                methods::SESSION_HISTORY,
                json!({ "session_id": sid, "after_seq": all.last().map_or(0, |e: &SessionEvent| e.seq) }),
            )
            .await
            .unwrap();
        all.extend(r.events);
        if !r.has_more {
            return all;
        }
    }
}

async fn list(td: &TestDaemon, c: &mut TestClient) -> Vec<SessionSummary> {
    let r: SessionListResult = c.call(td, methods::SESSION_LIST, json!({ "include_archived": true })).await.unwrap();
    r.sessions
}

async fn export(td: &TestDaemon, c: &mut TestClient, params: Value) -> Result<ExportResult, RpcError> {
    c.call(td, local_methods::EXPORT, params).await
}

async fn import(td: &TestDaemon, c: &mut TestClient, file: &Path, dry_run: bool) -> Result<ImportResult, RpcError> {
    c.call(td, local_methods::IMPORT, json!({ "file": file, "dry_run": dry_run })).await
}

async fn count(td: &TestDaemon, sql: &str, bind: &str) -> i64 {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(bind.to_string())
        .fetch_one(td.ctx.store.pool())
        .await
        .unwrap()
        .get::<i64, _>(0)
}

/// 全文索引里这个会话的行（seq、角色、文字）。
async fn fts_rows(td: &TestDaemon, sid: &str) -> Vec<(i64, String, String)> {
    sqlx::query("SELECT seq, role, text FROM messages_fts WHERE session_id = ? ORDER BY seq, role")
        .bind(sid)
        .fetch_all(td.ctx.store.pool())
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

async fn turn_rows(td: &TestDaemon, sid: &str) -> Vec<(i64, String, Option<String>)> {
    sqlx::query("SELECT turn_id, message_id, reason FROM turns WHERE session_id = ? ORDER BY turn_id")
        .bind(sid)
        .fetch_all(td.ctx.store.pool())
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

/// 把一个会话的活动时间改到过去（行和事件都改：「最后活动时间」取两者较大的）。
async fn backdate(td: &TestDaemon, sid: &str, ms: u64) {
    for sql in
        ["UPDATE sessions SET updated_ms = ?1 WHERE id = ?2", "UPDATE events SET ts_ms = ?1 WHERE session_id = ?2"]
    {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(ms as i64)
            .bind(sid)
            .execute(td.ctx.store.pool())
            .await
            .unwrap();
    }
}

fn read_zip(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut z = ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..z.len() {
        let mut e = z.by_index(i).unwrap();
        let mut buf = Vec::new();
        e.read_to_end(&mut buf).unwrap();
        out.insert(e.name().unwrap().into_owned(), buf);
    }
    out
}

fn write_zip(path: &Path, entries: &BTreeMap<String, Vec<u8>>) {
    let mut z = ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, body) in entries {
        z.start_file(name, SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)).unwrap();
        z.write_all(body).unwrap();
    }
    z.finish().unwrap();
}

/// 一台 daemon 里跑出来的合法导出：两个会话，第一个有项目和一张附件，第二个是已归档的 scratch 会话。
struct Fixture {
    a: TestDaemon,
    s1: String,
    s2: String,
    blob: AttachmentRef,
    png: Vec<u8>,
    zip: PathBuf,
}

impl Fixture {
    async fn new() -> Fixture {
        let a = TestDaemon::start().await;
        let mut c = a.client("a");
        let _: Value = c.call(&a, methods::WORKSPACE_CREATE, json!({ "name": "proj" })).await.unwrap();
        let png = png_fixture(640, 480, 3000, 1);
        let blob = upload(&a, &mut c, "shot.png", &png).await;
        let s1 = make_session(&a, &mut c, Some("proj"), "describe the zebra please", &[&blob]).await;
        let s2 = make_session(&a, &mut c, None, "tell me about the giraffe", &[]).await;
        let _: Empty = c.call(&a, methods::SESSION_ARCHIVE, json!({ "session_id": s2 })).await.unwrap();
        let out = a.dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let zip = out.join("backup.zip");
        let r = export(&a, &mut c, json!({ "out": zip, "include_archived": true })).await.unwrap();
        assert_eq!((r.sessions, r.blobs), (2, 1), "{r:?}");
        Fixture { a, s1, s2, blob, png, zip }
    }

    fn entries(&self) -> BTreeMap<String, Vec<u8>> {
        read_zip(&self.zip)
    }

    /// 改一份拷贝：`edit` 里改条目，写到 `<名字>.zip`，返回路径。
    fn variant(&self, name: &str, edit: impl FnOnce(&mut BTreeMap<String, Vec<u8>>)) -> PathBuf {
        let mut e = self.entries();
        edit(&mut e);
        let p = self.zip.with_file_name(format!("{name}.zip"));
        write_zip(&p, &e);
        p
    }

    fn session_json(&self, sid: &str) -> Value {
        serde_json::from_slice(&self.entries()[&format!("sessions/{sid}/session.json")]).unwrap()
    }
}

fn edit_json(e: &mut BTreeMap<String, Vec<u8>>, name: &str, f: impl FnOnce(&mut Value)) {
    let mut v: Value = serde_json::from_slice(&e[name]).unwrap();
    f(&mut v);
    e.insert(name.to_string(), serde_json::to_vec_pretty(&v).unwrap());
}

/// 导入之后什么都没留下：没有会话、事件、附件记录、全文索引、worktree 记录、turn，附件目录里没有文件，临时目录是空的。
async fn assert_nothing_left(td: &TestDaemon, sid: &str) {
    for table in ["sessions", "events", "turns", "messages_fts", "blobs", "worktrees"] {
        let col = if table == "sessions" { "id" } else { "session_id" };
        assert_eq!(count(td, &format!("SELECT COUNT(*) FROM {table} WHERE {col} = ?"), sid).await, 0, "{table}");
    }
    assert_blob_dir_clean(td, 0);
}

fn assert_blob_dir_clean(td: &TestDaemon, files: usize) {
    let dir = td.paths.blobs_dir();
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), files, "files in blobs/: {names:?}");
    let tmp: Vec<_> = std::fs::read_dir(dir.join("tmp")).unwrap().flatten().collect();
    assert!(tmp.is_empty(), "blobs/tmp is empty: {tmp:?}");
}

// ---------------------------------------------------------------- 往返

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_then_import_round_trips_events_attachments_search_and_archive_state() {
    let fx = Fixture::new().await;
    let mut ca = fx.a.client("a");
    let a_list = list(&fx.a, &mut ca).await;

    // 压缩包的内容和权限
    let names: Vec<String> = fx.entries().keys().cloned().collect();
    let blob_file = format!("sessions/{}/blobs/{}.png", fx.s1, fx.blob.blob_id);
    for want in [
        "manifest.json".to_string(),
        format!("sessions/{}/session.json", fx.s1),
        format!("sessions/{}/events.jsonl", fx.s1),
        blob_file.clone(),
        format!("sessions/{}/session.json", fx.s2),
        format!("sessions/{}/events.jsonl", fx.s2),
    ] {
        assert!(names.contains(&want), "{want} in {names:?}");
    }
    assert_eq!(names.len(), 6, "{names:?}");
    assert_eq!(fx.entries()[&blob_file], fx.png, "the attachment is stored as it is");
    assert_mode(&fx.zip, 0o600);
    let leftovers: Vec<_> = std::fs::read_dir(fx.zip.parent().unwrap()).unwrap().flatten().collect();
    assert_eq!(leftovers.len(), 1, "no temporary file stays behind: {leftovers:?}");
    let manifest: Value = serde_json::from_slice(&fx.entries()["manifest.json"]).unwrap();
    assert_eq!(manifest["format"], "anycon-export");
    assert_eq!(manifest["version"], 1);
    assert_eq!(manifest["session_count"], 2);
    assert_eq!(manifest["node_name"], "test-node");
    assert_eq!(manifest["anycon_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["filters"]["include_archived"], true);
    assert!(manifest["exported_at_ms"].as_u64().unwrap() > 1_700_000_000_000);
    let sj = fx.session_json(&fx.s1);
    assert_eq!(sj["project"], "proj");
    assert_eq!(sj["blobs"][0]["sha256"], sha256_hex(&fx.png));
    assert!(sj.get("project_loc").is_none() && sj.get("launch_args").is_none(), "machine-specific columns stay home");

    // 导入到另一台 daemon
    let b = TestDaemon::start().await;
    let mut cb = b.client("b");
    let r = import(&b, &mut cb, &fx.zip, false).await.unwrap();
    let mut want_ids = vec![fx.s1.clone(), fx.s2.clone()];
    want_ids.sort();
    let mut got = r.imported.clone();
    got.sort();
    assert_eq!(got, want_ids, "{r:?}");
    assert!(r.skipped.is_empty() && r.failed.is_empty(), "{r:?}");
    assert_eq!((r.total, r.blobs, r.source_node.as_str()), (2, 1, "test-node"));
    assert!(
        r.warnings.iter().any(|w| w.contains("proj")),
        "the project does not exist here, and the report says so: {:?}",
        r.warnings
    );
    assert_blob_dir_clean(&b, 1);

    // 会话列表：同样的 id、标题、归档状态；没有 agent 进程；agent_session_id 没有带过来；项目在这台机器上不存在也在
    let b_list = list(&b, &mut cb).await;
    assert_eq!(b_list.len(), 2);
    for a_s in &a_list {
        let b_s = b_list.iter().find(|s| s.id == a_s.id).expect("imported");
        assert_eq!(
            (&b_s.title, &b_s.project, b_s.archived, &b_s.settings, b_s.created_at_ms, b_s.updated_at_ms, b_s.last_seq),
            (&a_s.title, &a_s.project, a_s.archived, &a_s.settings, a_s.created_at_ms, a_s.updated_at_ms, a_s.last_seq)
        );
        assert_eq!(b_s.usage, a_s.usage);
        assert_eq!(b_s.preview, a_s.preview);
        assert_eq!(b_s.status, SessionStatus::Idle);
        assert_eq!(b_s.agent_session_id, None);
        assert!(
            a_s.agent_session_id.is_some(),
            "the original has one (the fake agent reports it) — so clearing is a choice"
        );
    }
    assert!(b_list.iter().find(|s| s.id == fx.s2).unwrap().archived);
    assert_eq!(b_list.iter().find(|s| s.id == fx.s1).unwrap().project.as_deref(), Some("proj"));

    // 事件逐条一样
    for sid in [&fx.s1, &fx.s2] {
        let (ha, hb) = (history(&fx.a, &mut ca, sid).await, history(&b, &mut cb, sid).await);
        assert!(!ha.is_empty());
        assert_eq!(ha, hb, "events of {sid}");
    }
    // 附件：记录、文件内容、权限、通过 blob.get 能下载
    let row = b.ctx.store.blob_get(&fx.blob.blob_id).await.unwrap().expect("blob row");
    assert_eq!(row.session_id.as_deref(), Some(fx.s1.as_str()));
    assert_eq!(
        (row.name.as_str(), row.mime.as_str(), row.size, row.sha256.as_str()),
        ("shot.png", "image/png", fx.png.len() as u64, sha256_hex(&fx.png).as_str())
    );
    assert_eq!((row.width, row.height), (Some(640), Some(480)));
    let file = b.paths.blobs_dir().join(format!("{}.png", fx.blob.blob_id));
    assert_eq!(std::fs::read(&file).unwrap(), fx.png);
    assert_mode(&file, 0o600);
    let got: BlobGetResult = cb
        .call(&b, methods::BLOB_GET, json!({ "blob_id": fx.blob.blob_id, "offset": 0, "max_bytes": 100000 }))
        .await
        .unwrap();
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(got.data).unwrap(), fx.png);
    assert_eq!(got.blob, fx.blob);
    // 全文索引：用户消息和 assistant 文字都在，和原来的行一致；搜得到
    for sid in [&fx.s1, &fx.s2] {
        let (fa, fb) = (fts_rows(&fx.a, sid).await, fts_rows(&b, sid).await);
        assert!(fa.iter().any(|r| r.1 == "user") && fa.iter().any(|r| r.1 == "assistant"), "{fa:?}");
        assert_eq!(fa, fb, "full-text rows of {sid}");
        assert_eq!(turn_rows(&fx.a, sid).await.len(), turn_rows(&b, sid).await.len());
        let (ta, tb) = (turn_rows(&fx.a, sid).await, turn_rows(&b, sid).await);
        assert_eq!(ta, tb, "turn rows of {sid}");
    }
    let hits: SessionSearchResult = cb.call(&b, methods::SESSION_SEARCH, json!({ "query": "zebra" })).await.unwrap();
    assert!(!hits.hits.is_empty() && hits.hits.iter().all(|h| h.session_id == fx.s1), "{hits:?}");
    let hits: SessionSearchResult = cb.call(&b, methods::SESSION_SEARCH, json!({ "query": "giraffe" })).await.unwrap();
    assert!(!hits.hits.is_empty() && hits.hits.iter().all(|h| h.session_id == fx.s2), "{hits:?}");
    // 在线客户端被通知有新会话
    assert!(
        cb.wait_notification(T, |n| n.method == methods::SESSION_CHANGED && n.params["session_id"] == json!(fx.s1))
            .await
            .is_some(),
        "session.changed for the imported session"
    );

    // 导入的会话还能继续：scratch 会话取消归档、发消息，agent 开的是新会话（没有 resume），历史接在后面
    let _: Empty = cb.call(&b, methods::SESSION_UNARCHIVE, json!({ "session_id": fx.s2 })).await.unwrap();
    let before = history(&b, &mut cb, &fx.s2).await.last().unwrap().seq;
    let _: SessionSendResult = cb
        .call(
            &b,
            methods::SESSION_SEND,
            json!({ "session_id": fx.s2, "message_id": "after-import", "content": "and the hippo?" }),
        )
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + T;
    loop {
        let evs = history(&b, &mut cb, &fx.s2).await;
        if evs.iter().any(|e| e.seq > before && matches!(e.event, Event::TurnDone { .. })) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the follow-up turn finishes");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let spawn = b.fake.spawns.lock().unwrap().last().cloned().expect("an agent process was started");
    assert_eq!(spawn.session_id, fx.s2);
    assert!(spawn.resume.is_none(), "a new agent session, not a resume of an id that is not on this machine");

    // 审计：只有数量和文件名
    let audit: NodeAuditResult = ca.call(&fx.a, methods::NODE_AUDIT, json!({ "limit": 100 })).await.unwrap();
    let e = audit.entries.iter().find(|e| e.action == "export").expect("export is audited");
    assert_eq!(e.target.as_deref(), Some("backup.zip"));
    assert_eq!((e.detail["sessions"].clone(), e.detail["blobs"].clone()), (json!(2), json!(1)));
    assert!(!e.detail.to_string().contains(fx.zip.parent().unwrap().to_str().unwrap()), "no path in the audit detail");
    let audit: NodeAuditResult = cb.call(&b, methods::NODE_AUDIT, json!({ "limit": 100 })).await.unwrap();
    let e = audit.entries.iter().find(|e| e.action == "import").expect("import is audited");
    assert_eq!(e.target.as_deref(), Some("backup.zip"));
    assert_eq!(
        (e.detail["imported"].clone(), e.detail["skipped"].clone(), e.detail["failed"].clone()),
        (json!(2), json!(0), json!(0))
    );

    b.shutdown().await;
    fx.a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn importing_again_skips_sessions_that_exist_and_touches_nothing() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    let mut cb = b.client("b");
    assert_eq!(import(&b, &mut cb, &fx.zip, false).await.unwrap().imported.len(), 2);
    let before = (history(&b, &mut cb, &fx.s1).await, fts_rows(&b, &fx.s1).await);
    let file = b.paths.blobs_dir().join(format!("{}.png", fx.blob.blob_id));
    let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();

    let r = import(&b, &mut cb, &fx.zip, false).await.unwrap();
    assert!(r.imported.is_empty() && r.failed.is_empty(), "{r:?}");
    assert_eq!(r.skipped.len(), 2);
    assert!(r.skipped.iter().all(|s| s.reason == "already_exists"));
    assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions WHERE id = ?", &fx.s1).await, 1);
    assert_eq!((history(&b, &mut cb, &fx.s1).await, fts_rows(&b, &fx.s1).await), before);
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), mtime, "the attachment file was not rewritten");
    assert_blob_dir_clean(&b, 1);

    // 导进导出它的那台 daemon 本身：全部跳过
    let mut ca = fx.a.client("a");
    let r = import(&fx.a, &mut ca, &fx.zip, false).await.unwrap();
    assert_eq!((r.imported.len(), r.skipped.len()), (0, 2));

    // 部分已存在：只导入缺的
    let c = TestDaemon::start().await;
    let mut cc = c.client("c");
    let only_s1 = fx.variant("only-s1", |e| {
        e.retain(|n, _| !n.contains(&fx.s2));
        edit_json(e, "manifest.json", |m| m["session_count"] = json!(1));
    });
    assert_eq!(import(&c, &mut cc, &only_s1, false).await.unwrap().imported, vec![fx.s1.clone()]);
    let r = import(&c, &mut cc, &fx.zip, false).await.unwrap();
    assert_eq!((r.imported, r.skipped.len()), (vec![fx.s2.clone()], 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dry_run_validates_and_reports_but_writes_nothing() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    let mut cb = b.client("b");
    let r = import(&b, &mut cb, &fx.zip, true).await.unwrap();
    assert!(r.dry_run);
    assert_eq!((r.imported.len(), r.skipped.len(), r.failed.len(), r.blobs), (2, 0, 0, 1), "{r:?}");
    assert!(r.events > 0);
    assert!(list(&b, &mut cb).await.is_empty());
    assert_eq!(count(&b, "SELECT COUNT(*) FROM events WHERE ?", "1").await, 0);
    assert_eq!(count(&b, "SELECT COUNT(*) FROM blobs WHERE ?", "1").await, 0);
    assert_blob_dir_clean(&b, 0);
    let audit: NodeAuditResult = cb.call(&b, methods::NODE_AUDIT, json!({ "limit": 100 })).await.unwrap();
    assert!(!audit.entries.iter().any(|e| e.action == "import"), "a dry run is not an import");

    // 干跑也照样校验：sha256 不符会被报出来
    let bad = fx.variant("dry-bad", |e| {
        let k = format!("sessions/{}/blobs/{}.png", fx.s1, fx.blob.blob_id);
        e.get_mut(&k).unwrap()[100] ^= 0xFF;
    });
    let r = import(&b, &mut cb, &bad, true).await.unwrap();
    assert_eq!((r.imported.len(), r.failed.len()), (1, 1));
    assert!(r.failed[0].error.contains("SHA-256"), "{:?}", r.failed);
    // 干跑看得出已经存在的
    let mut ca = fx.a.client("a");
    let r = import(&fx.a, &mut ca, &fx.zip, true).await.unwrap();
    assert_eq!((r.imported.len(), r.skipped.len()), (0, 2));
}

// ---------------------------------------------------------------- 筛选

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filters_pick_sessions_by_project_last_activity_id_and_archive_state() {
    let a = TestDaemon::start().await;
    let mut c = a.client("a");
    for p in ["p1", "p2"] {
        let _: Value = c.call(&a, methods::WORKSPACE_CREATE, json!({ "name": p })).await.unwrap();
    }
    let p1 = make_session(&a, &mut c, Some("p1"), "alpha one", &[]).await;
    let p1_archived = make_session(&a, &mut c, Some("p1"), "alpha two", &[]).await;
    let p2 = make_session(&a, &mut c, Some("p2"), "beta", &[]).await;
    let scratch = make_session(&a, &mut c, None, "gamma", &[]).await;
    let _: Empty = c.call(&a, methods::SESSION_ARCHIVE, json!({ "session_id": p1_archived })).await.unwrap();
    // p2 的最后活动在 2020 年
    let old = util::parse_utc("2020-01-01").unwrap();
    backdate(&a, &p2, old).await;
    let out = a.dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let mut n = 0;
    let mut run = |params: Value| {
        n += 1;
        let mut params = params;
        params["out"] = json!(out.join(format!("f{n}.zip")));
        (params, out.join(format!("f{n}.zip")))
    };
    let ids = |path: &Path| -> Vec<String> {
        let m: Value = serde_json::from_slice(&read_zip(path)["manifest.json"]).unwrap();
        let mut v: Vec<String> = read_zip(path)
            .keys()
            .filter_map(|k| k.strip_prefix("sessions/").and_then(|r| r.strip_suffix("/session.json")))
            .map(str::to_string)
            .collect();
        v.sort();
        assert_eq!(m["session_count"], v.len());
        v
    };
    let sorted = |mut v: Vec<&String>| -> Vec<String> {
        v.sort();
        v.into_iter().cloned().collect()
    };

    // 缺省：不含已归档的，但告诉你漏了几个
    let (p, f) = run(json!({}));
    let r = export(&a, &mut c, p).await.unwrap();
    assert_eq!((r.sessions, r.skipped_archived), (3, 1));
    assert_eq!(ids(&f), sorted(vec![&p1, &p2, &scratch]));
    // --all
    let (p, f) = run(json!({ "include_archived": true }));
    let r = export(&a, &mut c, p).await.unwrap();
    assert_eq!((r.sessions, r.skipped_archived), (4, 0));
    assert_eq!(ids(&f), sorted(vec![&p1, &p1_archived, &p2, &scratch]));
    // 项目
    let (p, f) = run(json!({ "project": "p1" }));
    let r = export(&a, &mut c, p).await.unwrap();
    assert_eq!((r.sessions, r.skipped_archived), (1, 1));
    assert_eq!(ids(&f), vec![p1.clone()]);
    let (p, f) = run(json!({ "project": "p1", "include_archived": true }));
    export(&a, &mut c, p).await.unwrap();
    assert_eq!(ids(&f), sorted(vec![&p1, &p1_archived]));
    let manifest: Value = serde_json::from_slice(&read_zip(&f)["manifest.json"]).unwrap();
    assert_eq!(manifest["filters"]["project"], "p1");
    let (p, f) = run(json!({ "project": "nope" }));
    let r = export(&a, &mut c, p).await.unwrap();
    assert_eq!(r.sessions, 0, "an empty selection is a valid, empty export");
    assert!(ids(&f).is_empty());
    // 最后活动时间
    let since = util::parse_utc("2026-01-01").unwrap();
    let (p, f) = run(json!({ "since_ms": since, "include_archived": true }));
    export(&a, &mut c, p).await.unwrap();
    assert_eq!(ids(&f), sorted(vec![&p1, &p1_archived, &scratch]), "p2 was last active in 2020");
    let (p, f) = run(json!({ "since_ms": old, "include_archived": true }));
    export(&a, &mut c, p).await.unwrap();
    assert_eq!(ids(&f).len(), 4);
    let (p, f) = run(json!({ "since_ms": util::now_ms() + 3_600_000, "include_archived": true }));
    assert_eq!(export(&a, &mut c, p).await.unwrap().sessions, 0);
    assert!(ids(&f).is_empty());
    // 点名：不受归档限制；多个；条件取交集
    let (p, f) = run(json!({ "sessions": [p1_archived] }));
    let r = export(&a, &mut c, p).await.unwrap();
    assert_eq!((r.sessions, r.skipped_archived), (1, 0));
    assert_eq!(ids(&f), vec![p1_archived.clone()]);
    let (p, f) = run(json!({ "sessions": [scratch, p2] }));
    export(&a, &mut c, p).await.unwrap();
    assert_eq!(ids(&f), sorted(vec![&p2, &scratch]));
    let (p, f) = run(json!({ "sessions": [scratch, p2], "project": "p2" }));
    export(&a, &mut c, p).await.unwrap();
    assert_eq!(ids(&f), vec![p2.clone()]);
    let manifest: Value = serde_json::from_slice(&read_zip(&f)["manifest.json"]).unwrap();
    assert_eq!(manifest["filters"]["sessions"].as_array().unwrap().len(), 2);
    // 点名了不存在的会话 / 不是会话 id
    let (p, _) = run(json!({ "sessions": ["sess_0000000000000000000000"] }));
    let e = export(&a, &mut c, p).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let (p, _) = run(json!({ "sessions": ["../../etc/passwd"] }));
    assert_eq!(code(&export(&a, &mut c, p).await.unwrap_err()), ErrorCode::Invalid);
}

// ---------------------------------------------------------------- 输出文件

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_output_file_is_checked_written_atomically_and_never_overwritten_by_accident() {
    let a = TestDaemon::start().await;
    let mut c = a.client("a");
    make_session(&a, &mut c, None, "hello", &[]).await;
    let dir = a.dir.path().join("out");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("x.zip");
    std::fs::write(&out, b"precious").unwrap();
    // 已经存在：拒绝，原文件不动
    let e = export(&a, &mut c, json!({ "out": out })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Exists, Some("output_exists")));
    assert_eq!(std::fs::read(&out).unwrap(), b"precious");
    // 允许覆盖
    let r = export(&a, &mut c, json!({ "out": out, "overwrite": true })).await.unwrap();
    assert_eq!(r.sessions, 1);
    assert_eq!(std::fs::metadata(&out).unwrap().len(), r.bytes);
    assert_mode(&out, 0o600);
    // 参数不合法
    for (p, why) in [
        (json!({ "out": "relative/x.zip" }), "relative"),
        (json!({ "out": "" }), "empty"),
        (json!({ "out": dir.join("no-such-dir").join("x.zip") }), "missing directory"),
        (json!({ "out": dir }), "a directory"),
        (json!({ "out": dir.join("y.zip"), "since_ms": u64::MAX }), "since"),
    ] {
        let e = export(&a, &mut c, p).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{why}: {e:?}");
    }
    let names: Vec<String> =
        std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["x.zip".to_string()], "failed attempts leave no files");

    // 同一时刻只有一个导出 / 导入
    let gate = a.ctx.transfer.begin().expect("free");
    let e = export(&a, &mut c, json!({ "out": dir.join("z.zip") })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    let e = import(&a, &mut c, &out, true).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    drop(gate);
    assert!(import(&a, &mut c, &out, true).await.is_ok());

    // 发起的连接已经断了（CLI 被 Ctrl+C）：导出取消、不留文件；导入不碰数据库
    let gone = a.client("gone");
    a.ctx.clients.unregister(gone.handle.conn_id);
    let mut gone = gone;
    let e = export(&a, &mut gone, json!({ "out": dir.join("g.zip") })).await.unwrap_err();
    assert!(e.message.contains("cancelled"), "{e:?}");
    assert!(!dir.join("g.zip").exists());
    let b = TestDaemon::start().await;
    let gone_b = b.client("gone");
    b.ctx.clients.unregister(gone_b.handle.conn_id);
    let mut gone_b = gone_b;
    assert!(import(&b, &mut gone_b, &out, false).await.is_err());
    assert!(list(&b, &mut b.client("x")).await.is_empty());
    assert_blob_dir_clean(&b, 0);
    let names: Vec<String> =
        std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["x.zip".to_string()]);

    // 导入：文件不存在 / 不是文件 / 相对路径
    let e = import(&b, &mut b.client("x"), &dir.join("nope.zip"), false).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let e = import(&b, &mut b.client("x"), &dir, false).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    let e = import(&b, &mut b.client("x"), Path::new("rel.zip"), false).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
}

// ---------------------------------------------------------------- 本机专用

/// 经 hub 来的客户端（`local = false`）直接走 RPC 调度器。
async fn remote_call(td: &TestDaemon, method: &str, params: Value) -> Result<Value, RpcError> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    struct Sink(tokio::sync::mpsc::UnboundedSender<RpcNotification>);
    impl anycon_daemon::clients::ClientSink for Sink {
        fn send(&self, n: RpcNotification) {
            let _ = self.0.send(n);
        }
    }
    let caller =
        Caller { device_id: "remote-1".into(), device_name: "Phone".into(), kind: DeviceKind::Web, local: false };
    let handle: std::sync::Arc<ClientHandle> = td.ctx.clients.register(caller, Box::new(Sink(tx)));
    let req = RpcRequest { jsonrpc: "2.0".into(), id: RpcId::Num(1), method: method.into(), params };
    match td.rpc().handle_request(&handle, req).await {
        RpcMessage::Response(r) => match r.error {
            Some(e) => Err(e),
            None => Ok(r.result.unwrap_or(Value::Null)),
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_client_cannot_export_or_import() {
    let fx = Fixture::new().await;
    let dir = fx.a.dir.path().join("remote");
    std::fs::create_dir_all(&dir).unwrap();
    let e = remote_call(&fx.a, local_methods::EXPORT, json!({ "out": dir.join("stolen.zip") })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden, "{e:?}");
    assert!(e.message.contains("local socket"), "{e:?}");
    assert!(!dir.join("stolen.zip").exists());
    let b = TestDaemon::start().await;
    let e = remote_call(&b, local_methods::IMPORT, json!({ "file": fx.zip })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden, "{e:?}");
    assert!(list(&b, &mut b.client("x")).await.is_empty(), "nothing was imported");
    let e = remote_call(&b, local_methods::IMPORT, json!({ "file": fx.zip, "dry_run": true })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Forbidden);
    // 没有留下审计
    let audit = b.ctx.audit.query(None, None, 100).await;
    assert!(!audit.iter().any(|e| e.action == "import" || e.action == "export"));
}

// ---------------------------------------------------------------- 进度

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_goes_only_to_the_connection_that_asked() {
    let fx = Fixture::new().await;
    let mut ca = fx.a.client("asker");
    let mut other = fx.a.client("other");
    let out = fx.a.dir.path().join("out").join("p.zip");
    export(&fx.a, &mut ca, json!({ "out": out, "include_archived": true })).await.unwrap();
    let notes = ca.drain_now();
    let progress: Vec<Progress> = notes
        .iter()
        .filter(|n| n.method == PROGRESS_METHOD)
        .map(|n| serde_json::from_value(n.params.clone()).unwrap())
        .collect();
    let last = progress.last().expect("progress was reported");
    assert_eq!((last.op.as_str(), last.done, last.total), ("export", 2, 2));
    assert!(last.bytes > 0);
    assert!(other.drain_now().iter().all(|n| n.method != PROGRESS_METHOD), "other clients hear nothing");

    let b = TestDaemon::start().await;
    let mut cb = b.client("asker");
    import(&b, &mut cb, &out, false).await.unwrap();
    let last: Progress = cb
        .drain_now()
        .iter()
        .filter(|n| n.method == PROGRESS_METHOD)
        .map(|n| serde_json::from_value(n.params.clone()).unwrap())
        .next_back()
        .expect("import progress");
    assert_eq!((last.op.as_str(), last.done, last.total), ("import", 2, 2));
}

// ---------------------------------------------------------------- 恶意压缩包

/// 导入被整个拒绝：`invalid`，什么都没写。
async fn assert_rejected(b: &TestDaemon, file: &Path, why: &str) -> RpcError {
    let mut c = b.client("b");
    let e = import(b, &mut c, file, false).await.expect_err(&format!("{why}: must be rejected"));
    assert_eq!(code(&e), ErrorCode::Invalid, "{why}: {e:?}");
    assert!(list(b, &mut c).await.is_empty(), "{why}: nothing was imported");
    assert_eq!(count(b, "SELECT COUNT(*) FROM events WHERE ?", "1").await, 0, "{why}");
    assert_eq!(count(b, "SELECT COUNT(*) FROM blobs WHERE ?", "1").await, 0, "{why}");
    assert_blob_dir_clean(b, 0);
    e
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archives_with_paths_outside_the_format_are_rejected_whole() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    let evil = b.dir.path().join("evil.txt");
    // zip-slip、绝对路径、反斜杠、Windows 盘符、多余的文件、macOS 重新打包带的垃圾
    for (i, name) in [
        "../evil.txt",
        "sessions/../../evil.txt",
        "/etc/cron.d/evil",
        "sessions\\evil.txt",
        "C:/Windows/evil.txt",
        "evil.txt",
        "__MACOSX/._manifest.json",
        ".DS_Store",
        "sessions/sess_short/session.json",
        "sessions/sess_0123456789abcdefABCDEF/blobs/../../../evil.txt",
        "sessions/sess_0123456789abcdefABCDEF/blobs/evil.png",
    ]
    .into_iter()
    .enumerate()
    {
        let f = fx.variant(&format!("slip-{i}"), |e| {
            e.insert(name.to_string(), b"owned".to_vec());
        });
        let e = assert_rejected(&b, &f, name).await;
        assert!(e.message.contains("unexpected entry"), "{name}: {e:?}");
        assert_eq!(reason(&e), Some("bad_archive"));
    }
    assert!(!evil.exists());
    assert!(!b.dir.path().parent().unwrap().join("evil.txt").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symbolic_links_are_rejected() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    let blob_name = format!("sessions/{}/blobs/{}.png", fx.s1, fx.blob.blob_id);
    // 附件的位置是个指向 /etc/passwd 的符号链接
    let mut e = fx.entries();
    e.remove(&blob_name);
    let p = fx.zip.with_file_name("symlink.zip");
    let mut z = ZipWriter::new(std::fs::File::create(&p).unwrap());
    for (name, body) in &e {
        z.start_file(name, SimpleFileOptions::default()).unwrap();
        z.write_all(body).unwrap();
    }
    z.add_symlink(&blob_name, "/etc/passwd", SimpleFileOptions::default()).unwrap();
    z.finish().unwrap();
    let err = assert_rejected(&b, &p, "symlink").await;
    assert!(err.message.contains("not a regular file"), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_formats_versions_and_broken_archives_are_rejected() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    // 版本不认识
    for v in [0, 2, 99] {
        let f = fx.variant(&format!("v{v}"), |e| edit_json(e, "manifest.json", |m| m["version"] = json!(v)));
        let e = assert_rejected(&b, &f, "version").await;
        assert_eq!(reason(&e), Some("unsupported_version"), "{e:?}");
        assert!(e.message.contains(&format!("version {v}")), "{e:?}");
    }
    // 版本不认识时条目结构也可能不一样：报版本，不报多出的文件
    let f = fx.variant("v2-layout", |e| {
        edit_json(e, "manifest.json", |m| m["version"] = json!(2));
        e.insert("data/everything.bin".into(), vec![1, 2, 3]);
    });
    assert_eq!(reason(&assert_rejected(&b, &f, "v2 layout").await), Some("unsupported_version"));
    // format 不对
    let f = fx.variant("format", |e| edit_json(e, "manifest.json", |m| m["format"] = json!("something-else")));
    let e = assert_rejected(&b, &f, "format").await;
    assert!(e.message.contains("not an anycon export"), "{e:?}");
    // manifest 缺失 / 不是 JSON / session_count 对不上
    let f = fx.variant("no-manifest", |e| {
        e.remove("manifest.json");
    });
    assert!(assert_rejected(&b, &f, "no manifest").await.message.contains("manifest.json is missing"));
    let f = fx.variant("junk-manifest", |e| {
        e.insert("manifest.json".into(), b"{ not json".to_vec());
    });
    assert_rejected(&b, &f, "junk manifest").await;
    let f = fx.variant("count", |e| edit_json(e, "manifest.json", |m| m["session_count"] = json!(3)));
    assert!(assert_rejected(&b, &f, "count").await.message.contains("holds 2"));
    // 会话缺 events.jsonl / session.json
    let f = fx.variant("no-events", |e| {
        e.remove(&format!("sessions/{}/events.jsonl", fx.s1));
    });
    assert_rejected(&b, &f, "no events").await;
    let f = fx.variant("no-session-json", |e| {
        e.remove(&format!("sessions/{}/session.json", fx.s1));
    });
    assert_rejected(&b, &f, "no session.json").await;
    // 不是 zip / 被截断 / 空文件
    let junk = fx.zip.with_file_name("junk.zip");
    std::fs::write(&junk, b"definitely not a zip file").unwrap();
    assert_rejected(&b, &junk, "junk").await;
    let bytes = std::fs::read(&fx.zip).unwrap();
    let cut = fx.zip.with_file_name("cut.zip");
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    assert_rejected(&b, &cut, "truncated").await;
    let empty = fx.zip.with_file_name("empty.zip");
    std::fs::write(&empty, b"").unwrap();
    assert_rejected(&b, &empty, "empty").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_limits_stop_oversized_archives_before_anything_is_written() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    let mut c = b.client("b");
    let defaults = Limits::default();
    let total: u64 = fx.entries().values().map(|v| v.len() as u64).sum();
    for (limits, what) in [
        (Limits { max_total_bytes: total - 1, ..defaults.clone() }, "total"),
        (Limits { max_blob_bytes: fx.png.len() as u64 - 1, ..defaults.clone() }, "attachment"),
        (Limits { max_events_bytes: 10, ..defaults.clone() }, "events"),
        (Limits { max_session_json_bytes: 10, ..defaults.clone() }, "session.json"),
        (Limits { max_manifest_bytes: 10, ..defaults.clone() }, "manifest"),
        (Limits { max_entries: 3, ..defaults.clone() }, "entries"),
    ] {
        b.ctx.transfer.set_limits(limits);
        let e = import(&b, &mut c, &fx.zip, false).await.expect_err(what);
        assert_eq!(code(&e), ErrorCode::Invalid, "{what}: {e:?}");
        assert!(list(&b, &mut c).await.is_empty(), "{what}");
        assert_blob_dir_clean(&b, 0);
    }
    // 一行事件太长：只让这个会话失败（整个事务回滚），别的会话照常
    b.ctx.transfer.set_limits(Limits { max_event_line_bytes: 120, ..defaults.clone() });
    let r = import(&b, &mut c, &fx.zip, false).await.unwrap();
    assert_eq!(r.failed.len(), 2, "{r:?}");
    assert!(r.failed.iter().all(|f| f.error.contains("longer than 120 bytes")), "{r:?}");
    assert_nothing_left(&b, &fx.s1).await;
    assert_nothing_left(&b, &fx.s2).await;
    // 放宽之后同一份文件导得进去
    b.ctx.transfer.set_limits(defaults);
    assert_eq!(import(&b, &mut c, &fx.zip, false).await.unwrap().imported.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_attachment_fails_its_session_and_leaves_no_half_session() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    let mut c = b.client("b");
    let blob_name = format!("sessions/{}/blobs/{}.png", fx.s1, fx.blob.blob_id);

    // 内容被改（SHA-256 对不上）
    let f = fx.variant("sha", |e| e.get_mut(&blob_name).unwrap()[100] ^= 0xFF);
    let r = import(&b, &mut c, &f, false).await.unwrap();
    assert_eq!(r.imported, vec![fx.s2.clone()], "the other session is fine: {r:?}");
    assert_eq!(r.failed.len(), 1);
    assert_eq!(r.failed[0].id, fx.s1);
    assert!(r.failed[0].error.contains("SHA-256"), "{:?}", r.failed);
    assert_nothing_left(&b, &fx.s1).await;
    assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions WHERE id = ?", &fx.s2).await, 1);

    // 会话里声明的 sha256 被改
    let f = fx.variant("sha-declared", |e| {
        edit_json(e, &format!("sessions/{}/session.json", fx.s1), |s| s["blobs"][0]["sha256"] = json!("0".repeat(64)))
    });
    let r = import(&b, &mut c, &f, false).await.unwrap();
    assert_eq!((r.failed.len(), r.skipped.len()), (1, 1));
    assert_nothing_left(&b, &fx.s1).await;

    // 大小对不上：条目比声明的短 / 长
    for (i, body) in [fx.png[..fx.png.len() - 1].to_vec(), [fx.png.clone(), vec![0]].concat()].into_iter().enumerate() {
        let f = fx.variant(&format!("size-{i}"), |e| {
            e.insert(blob_name.clone(), body);
        });
        let r = import(&b, &mut c, &f, false).await.unwrap();
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert!(r.failed[0].error.contains("size"), "{:?}", r.failed);
        assert_nothing_left(&b, &fx.s1).await;
    }
    // 压缩包里有 session.json 没列出的附件 / 列出了却没有文件
    let f = fx.variant("unlisted", |e| {
        e.insert(format!("sessions/{}/blobs/{}.png", fx.s1, "f".repeat(32)), vec![1, 2, 3]);
    });
    assert_eq!(import(&b, &mut c, &f, false).await.unwrap().failed.len(), 1);
    let f = fx.variant("missing", |e| {
        e.remove(&blob_name);
    });
    let r = import(&b, &mut c, &f, false).await.unwrap();
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].error.contains("do not match"), "{:?}", r.failed);
    assert_nothing_left(&b, &fx.s1).await;

    // 没改过的照样导得进去
    let r = import(&b, &mut c, &fx.zip, false).await.unwrap();
    assert_eq!((r.imported, r.skipped.len()), (vec![fx.s1.clone()], 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_event_stream_rolls_the_whole_session_back_including_attachments() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    let mut c = b.client("b");
    let events = format!("sessions/{}/events.jsonl", fx.s1);
    let lines = |fx: &Fixture| -> Vec<String> {
        String::from_utf8(fx.entries()[&events].clone()).unwrap().lines().map(str::to_string).collect()
    };
    let good = lines(&fx);
    assert!(good.len() > 4);
    let mut cases: Vec<(&str, String)> = Vec::new();
    // 最后一行不是合法事件（前面的行已经写进事务了）
    cases.push(("garbage at the end", format!("{}\n{{ nope\n", good.join("\n"))));
    // 事件属于别的会话
    let mut wrong = good.clone();
    wrong[2] = wrong[2].replace(&fx.s1, &fx.s2);
    cases.push(("another session", wrong.join("\n") + "\n"));
    // seq 不递增
    let mut dup = good.clone();
    dup.insert(3, good[2].clone());
    cases.push(("repeated seq", dup.join("\n") + "\n"));
    // 少了一行（和 session.json 里的 event_count 对不上）
    cases.push(("missing event", good[..good.len() - 1].join("\n") + "\n"));
    // 事件类型不认识
    let mut unknown = good.clone();
    let at = good.iter().position(|l| l.contains("\"type\":\"turn_start\"")).expect("a turn_start event");
    unknown[at] = unknown[at].replace("\"type\":\"turn_start\"", "\"type\":\"from_the_future\"");
    cases.push(("unknown event type", unknown.join("\n") + "\n"));
    for (i, (what, body)) in cases.into_iter().enumerate() {
        let f = fx.variant(&format!("ev-{i}"), |e| {
            e.insert(events.clone(), body.into_bytes());
        });
        let r = import(&b, &mut c, &f, false).await.unwrap();
        assert_eq!(r.failed.len(), 1, "{what}: {r:?}");
        assert_eq!(r.failed[0].id, fx.s1, "{what}");
        assert_nothing_left(&b, &fx.s1).await;
        assert!(
            r.imported.contains(&fx.s2) || r.skipped.iter().any(|s| s.id == fx.s2),
            "{what}: the other session is unaffected"
        );
    }
    // 空行和行尾的 \r 是允许的
    let f = fx.variant("crlf", |e| {
        e.insert(events.clone(), (good.join("\r\n") + "\r\n\r\n").into_bytes());
    });
    let r = import(&b, &mut c, &f, false).await.unwrap();
    assert_eq!(r.imported, vec![fx.s1.clone()], "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_json_fields_are_validated_one_by_one() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    let mut c = b.client("b");
    let key = format!("sessions/{}/session.json", fx.s1);
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let wt = |rel: &str, branch: &str, base: &str, base_sha: &str| json!({ "branch": branch, "base": base, "base_display": "main", "base_sha": base_sha, "rel_path": rel, "state": "active", "created_ms": 1 });
    type Edit<'a> = Box<dyn Fn(&mut Value) + 'a>;
    let cases: Vec<(&str, Edit<'_>)> = vec![
        ("id", Box::new(|s| s["id"] = json!(format!("sess_{}", "a".repeat(22))))),
        ("project dot dot", Box::new(|s| s["project"] = json!("../x"))),
        ("project slash", Box::new(|s| s["project"] = json!("a/b"))),
        ("project hidden", Box::new(|s| s["project"] = json!(".trash"))),
        ("agent", Box::new(|s| s["agent"] = json!("claude code; rm"))),
        ("title", Box::new(|s| s["title"] = json!("t".repeat(5000)))),
        ("future", Box::new(|s| s["updated_ms"] = json!(u64::MAX))),
        ("status", Box::new(|s| s["status"] = json!("exploded"))),
        ("settings", Box::new(|s| s["settings"] = json!("not an object"))),
        ("blob id", Box::new(|s| s["blobs"][0]["blob_id"] = json!("../../x"))),
        ("blob file", Box::new(|s| s["blobs"][0]["file"] = json!("../../x.png"))),
        ("blob file other id", Box::new(|s| s["blobs"][0]["file"] = json!(format!("{}.png", "e".repeat(32))))),
        ("blob name", Box::new(|s| s["blobs"][0]["name"] = json!("a/b.png"))),
        ("blob name bidi", Box::new(|s| s["blobs"][0]["name"] = json!("invoice\u{202E}gnp.exe"))),
        ("blob mime", Box::new(|s| s["blobs"][0]["mime"] = json!("image/png\r\nX: y"))),
        ("blob sha", Box::new(|s| s["blobs"][0]["sha256"] = json!("ABC"))),
        ("blob ext", Box::new(|s| s["blobs"][0]["file"] = json!(format!("{}.p ng", fx.blob.blob_id)))),
        ("event_count", Box::new(|s| s["event_count"] = json!(1))),
        ("last_seq", Box::new(|s| s["last_seq"] = json!(1))),
        ("wt escape", Box::new(move |s| s["worktree"] = wt("../../etc", "b", "main", sha))),
        ("wt dotdot", Box::new(move |s| s["worktree"] = wt(".anycon/worktrees/..", "b", "main", sha))),
        ("wt branch option", Box::new(move |s| s["worktree"] = wt(".anycon/worktrees/x", "-D", "main", sha))),
        ("wt base option", Box::new(move |s| s["worktree"] = wt(".anycon/worktrees/x", "b", "--output=x", sha))),
        ("wt sha", Box::new(move |s| s["worktree"] = wt(".anycon/worktrees/x", "b", "main", "main"))),
    ];
    for (what, edit) in cases {
        let f = fx.variant("field", |e| edit_json(e, &key, |s| edit(s)));
        let r = import(&b, &mut c, &f, false).await.unwrap();
        assert!(r.failed.iter().any(|x| x.id == fx.s1), "{what}: must fail: {r:?}");
        assert_nothing_left(&b, &fx.s1).await;
    }
    // 一个没有项目的会话不能有 worktree
    let f = fx.variant("scratch-wt", |e| {
        edit_json(e, &format!("sessions/{}/session.json", fx.s2), |s| {
            s["worktree"] = wt(".anycon/worktrees/x", "b", "main", sha)
        })
    });
    let r = import(&b, &mut c, &f, false).await.unwrap();
    assert!(r.failed.iter().any(|x| x.id == fx.s2), "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attachment_id_that_is_taken_fails_the_session_and_leaves_the_existing_file_alone() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start().await;
    let mut c = b.client("b");
    // 这台 daemon 上已经有一个同 id 的附件（别的会话用着）
    let existing = b"the other file".to_vec();
    let file = b.paths.blobs_dir().join(format!("{}.png", fx.blob.blob_id));
    std::fs::write(&file, &existing).unwrap();
    b.ctx
        .store
        .blob_insert(&anycon_daemon::store::BlobRow {
            id: fx.blob.blob_id.clone(),
            session_id: Some("sess_other".into()),
            name: "x.png".into(),
            mime: "image/png".into(),
            size: existing.len() as u64,
            sha256: sha256_hex(&existing),
            width: None,
            height: None,
            path: format!("{}.png", fx.blob.blob_id),
            created_ms: 1,
        })
        .await
        .unwrap();
    let r = import(&b, &mut c, &fx.zip, false).await.unwrap();
    assert_eq!((r.failed.len(), r.imported.len()), (1, 1), "{r:?}");
    assert_eq!(r.failed[0].id, fx.s1);
    assert!(r.failed[0].error.contains("already exists"), "{:?}", r.failed);
    assert_eq!(std::fs::read(&file).unwrap(), existing, "the other attachment is untouched");
    assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions WHERE id = ?", &fx.s1).await, 0);
    assert_eq!(count(&b, "SELECT COUNT(*) FROM events WHERE session_id = ?", &fx.s1).await, 0);
    assert_eq!(
        b.ctx.store.blob_get(&fx.blob.blob_id).await.unwrap().unwrap().session_id.as_deref(),
        Some("sess_other")
    );
}

// ---------------------------------------------------------------- 导入的会话是什么状态

fn row(id: &str, project: Option<&str>, status: SessionStatus) -> SessionRow {
    SessionRow {
        id: id.into(),
        project: project.map(Into::into),
        project_loc: project.map(|p| format!("linked:/some/where/{p}")),
        agent: "fake".into(),
        title: "crafted".into(),
        title_locked: true,
        status,
        archived: false,
        settings: SessionSettings { model: Some("fake-large".into()), ..Default::default() },
        agent_session_id: Some("native-session-id".into()),
        launch_args: json!({ "model": "fake-large", "mcp_config": "/home/other/.mcp.json" }),
        created_ms: 1_700_000_000_000,
        updated_ms: 1_700_000_100_000,
        last_seq: 0,
        next_turn_id: 2,
        usage: UsageTotals::default(),
        preview: Some("hi".into()),
        worktree_id: None,
    }
}

async fn put_event(td: &TestDaemon, sid: &str, seq: u64, turn: Option<u64>, event: Event) {
    td.ctx
        .store
        .append_event(&SessionEvent {
            session_id: sid.into(),
            seq,
            ts_ms: 1_700_000_000_000 + seq,
            turn_id: turn,
            event,
        })
        .await
        .unwrap();
}

fn wt_row(id: &str, session: &str, rel: &str, branch: &str, state: WorktreeState) -> WorktreeRow {
    WorktreeRow {
        id: id.into(),
        project_loc: Some("linked:/some/where/proj".into()),
        project: "proj".into(),
        rel_path: rel.into(),
        branch: branch.into(),
        base: "refs/heads/main".into(),
        base_display: "main".into(),
        base_sha: "0123456789abcdef0123456789abcdef01234567".into(),
        session_id: Some(session.into()),
        state,
        identity: Some((7, 99)),
        git_pointer: Some("gitdir: {project}/.git/worktrees/x".into()),
        created_ms: 1_700_000_000_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn imported_sessions_that_were_running_are_interrupted_and_their_open_requests_are_closed() {
    let a = TestDaemon::start().await;
    let sid = format!("sess_{}", "r".repeat(22));
    // 导出时正等着权限答复
    let mut r = row(&sid, None, SessionStatus::WaitingPermission);
    r.last_seq = 4;
    a.ctx.store.upsert_session(&r).await.unwrap();
    put_event(
        &a,
        &sid,
        1,
        None,
        Event::UserMessage {
            message_id: "m1".into(),
            content: "do it".into(),
            by_device: "d".into(),
            attachments: vec![],
        },
    )
    .await;
    put_event(&a, &sid, 2, Some(1), Event::TurnStart { turn_id: 1, message_id: "m1".into() }).await;
    put_event(
        &a,
        &sid,
        3,
        Some(1),
        Event::Permission {
            request_id: "req-1".into(),
            kind: PermissionKind::Tool,
            title: "Run".into(),
            detail: PermissionDetail::default(),
            options: vec![],
            expires_at_ms: None,
        },
    )
    .await;
    put_event(&a, &sid, 4, Some(1), Event::TextDelta { text: "partial answer about narwhals".into() }).await;
    let mut c = a.client("a");
    let zip = a.dir.path().join("running.zip");
    let ex = export(&a, &mut c, json!({ "out": zip })).await.unwrap();
    assert_eq!(ex.sessions, 1, "the exporting side does not care what state it is in");
    assert_eq!(
        a.ctx.store.get_session(&sid).await.unwrap().unwrap().updated_ms,
        1_700_000_100_000,
        "export reads, never writes"
    );

    let b = TestDaemon::start().await;
    let mut cb = b.client("b");
    let res = import(&b, &mut cb, &zip, false).await.unwrap();
    assert_eq!(res.imported, vec![sid.clone()], "{res:?}");
    let s: SessionSummary = cb.call(&b, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
    assert_eq!(s.status, SessionStatus::Interrupted);
    assert_eq!((s.agent_session_id, s.pending_permissions), (None, 0));
    let evs = history(&b, &mut cb, &sid).await;
    assert_eq!(evs.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5, 6]);
    assert!(
        matches!(&evs[4].event, Event::PermissionResolved { request_id, option_id: None, by_device, .. } if request_id == "req-1" && by_device == "daemon"),
        "{:?}",
        evs[4]
    );
    assert!(matches!(evs[5].event, Event::Status { state: SessionStatus::Interrupted, .. }), "{:?}", evs[5]);
    assert_eq!(s.last_seq, 6);
    let pending: PermissionListResult = cb.call(&b, methods::PERMISSION_LIST, json!({})).await.unwrap();
    assert!(pending.requests.is_empty());
    assert!(b.ctx.store.unresolved_permissions(&sid).await.unwrap().is_empty());
    // 没结束的 turn 收尾；下一个 turn 的编号接在后面；列也按文件里的来
    assert_eq!(turn_rows(&b, &sid).await, vec![(1, "m1".to_string(), Some("error".to_string()))]);
    let row = b.ctx.store.get_session(&sid).await.unwrap().unwrap();
    assert_eq!((row.next_turn_id, row.last_seq), (2, 6));
    assert_eq!((row.title.as_str(), row.title_locked, row.preview.as_deref()), ("crafted", true, Some("hi")));
    assert_eq!((row.project_loc, row.launch_args), (None, Value::Null), "machine-specific columns are not imported");
    // 没写完的 assistant 文字也进了全文索引
    let hits: SessionSearchResult = cb.call(&b, methods::SESSION_SEARCH, json!({ "query": "narwhals" })).await.unwrap();
    assert_eq!(hits.hits.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_sessions_come_back_with_unusable_directories_and_only_the_metadata() {
    let a = TestDaemon::start().await;
    let (live, gone, failed) =
        (format!("sess_{}", "w".repeat(22)), format!("sess_{}", "x".repeat(22)), format!("sess_{}", "y".repeat(22)));
    for (sid, wid, rel, branch, state, archived) in [
        (&live, "wt_live", ".anycon/worktrees/live", "anycon/live", WorktreeState::Active, false),
        (&gone, "wt_gone", ".anycon/worktrees/gone", "anycon/gone", WorktreeState::Removed, true),
        (&failed, "wt_failed", ".anycon/worktrees/failed", "anycon/failed", WorktreeState::Failed, false),
    ] {
        let mut r = row(sid, Some("proj"), SessionStatus::Idle);
        r.worktree_id = Some(wid.into());
        r.archived = archived;
        a.ctx.store.insert_session_with_worktree(&r, &wt_row(wid, sid, rel, branch, state)).await.unwrap();
        put_event(
            &a,
            sid,
            1,
            None,
            Event::UserMessage {
                message_id: "m".into(),
                content: "hi".into(),
                by_device: "d".into(),
                attachments: vec![],
            },
        )
        .await;
    }
    let mut c = a.client("a");
    let zip = a.dir.path().join("wt.zip");
    export(&a, &mut c, json!({ "out": zip, "include_archived": true })).await.unwrap();
    // 只有元数据：没有设备号 / inode、没有 .git 指针、没有项目位置
    let e = read_zip(&zip);
    let live_json: Value = serde_json::from_slice(&e[&format!("sessions/{live}/session.json")]).unwrap();
    assert_eq!(live_json["worktree"]["branch"], "anycon/live");
    assert_eq!(live_json["worktree"]["rel_path"], ".anycon/worktrees/live");
    let text = live_json.to_string();
    for secret in ["project_loc", "git_pointer", "identity", "gitdir", "/some/where", "mcp_config"] {
        assert!(!text.contains(secret), "{secret} must not be exported: {text}");
    }

    let b = TestDaemon::start().await;
    let mut cb = b.client("b");
    let r = import(&b, &mut cb, &zip, false).await.unwrap();
    assert_eq!((r.imported.len(), r.failed.len()), (3, 0), "{r:?}");
    for (sid, state) in
        [(&live, WorktreeState::Missing), (&gone, WorktreeState::Removed), (&failed, WorktreeState::Failed)]
    {
        let s: SessionSummary = cb.call(&b, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
        let w = s.worktree.expect("the worktree record came along");
        assert_eq!(w.state, state, "{sid}");
        assert_eq!((w.base.as_str(), w.base_sha.len()), ("main", 40));
        let wid = b.ctx.store.get_session(sid).await.unwrap().unwrap().worktree_id.unwrap();
        assert!(!["wt_live", "wt_gone", "wt_failed"].contains(&wid.as_str()), "a fresh worktree id");
        let row = b.ctx.store.get_worktree(&wid).await.unwrap().unwrap();
        assert_eq!((row.identity, row.git_pointer, row.project_loc), (None, None, None), "{sid}");
        assert_eq!(row.session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(row.project, "proj");
    }
    // 没有凭空出现 worktree 目录
    assert!(!b.workspace.join("proj").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_report_warns_when_retention_would_delete_what_was_just_imported() {
    let fx = Fixture::new().await;
    let b = TestDaemon::start_with(&[("sessions.history_retention", "\"1h\"")], false).await;
    let mut cb = b.client("b");
    // 一个很老的会话，一个刚刚才有活动的
    let old = fx.variant("old", |e| {
        edit_json(e, &format!("sessions/{}/session.json", fx.s2), |s| s["updated_ms"] = json!(1_600_000_000_000u64));
    });
    let r = import(&b, &mut cb, &old, true).await.unwrap();
    assert!(
        r.warnings.iter().all(|w| !w.contains("history_retention")),
        "a dry run deletes nothing, so it does not warn about it: {r:?}"
    );
    let r = import(&b, &mut cb, &old, false).await.unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("history_retention") && w.contains("1 of")), "{:?}", r.warnings);
}
