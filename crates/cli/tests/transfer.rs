//! `anycon export` / `anycon import`（docs/spec/cli.md § 3.6）：真实的 `anycon` 二进制加真实的 daemon 子进程（mock agent，
//! 本机 socket）——两台 daemon 之间搬会话、默认文件名、筛选、退出码、daemon 没在跑时的提示。
//! 导出格式和校验的细节在 `crates/daemon/tests/transfer.rs`。

mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use common::*;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

/// 初始化一个 home（关掉真实 agent），起 daemon（mock agent）。
async fn node(root: &Path, name: &str, dir: &str) -> (PathBuf, DaemonProc) {
    let home = root.join(dir);
    init(&home, &root.join(format!("ws-{dir}")), name, None).await;
    for agent in ["claude-code", "codex"] {
        assert_eq!(
            anycon(&home, &["config", "set", &format!("agents.{agent}.enabled"), "false"], None, &[]).await.code,
            0
        );
    }
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;
    (home, daemon)
}

fn zip_entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut z = ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    (0..z.len())
        .map(|i| {
            let mut e = z.by_index(i).unwrap();
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).unwrap();
            (e.name().unwrap().into_owned(), buf)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_move_between_two_daemons_through_the_binary() {
    let d = tempfile::tempdir().unwrap();
    let (a, daemon_a) = node(d.path(), "alpha box", "a").await;
    macro_rules! a {
        ($($x:expr),* $(,)?) => { anycon(&a, &[$($x),*], None, &[]) };
    }
    assert_eq!(a!("projects", "create", "demo").await.code, 0);
    let r = a!("new", "--agent", "mock", "--project", "demo", "say hello to the zebra").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let r = a!("new", "--agent", "mock", "what about a giraffe").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let ls = a!("ls", "--json").await.json();
    let sessions = ls["sessions"].as_array().unwrap().clone();
    assert_eq!(sessions.len(), 2);
    let demo_id = sessions.iter().find(|s| s["project"] == "demo").unwrap()["id"].as_str().unwrap().to_string();
    let scratch_id = sessions.iter().find(|s| s["project"].is_null()).unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(a!("archive", scratch_id.as_str()).await.code, 0);

    // 默认文件名：当前目录里的 anycon-export-<节点名>-<日期>.zip；已归档的不带，但告诉你
    let cwd = d.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let r = anycon_in(&a, Some(&cwd), &["export"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let default_name =
        std::fs::read_dir(&cwd).unwrap().flatten().next().expect("a file").file_name().to_string_lossy().into_owned();
    assert!(
        default_name.starts_with("anycon-export-alpha-box-")
            && default_name.ends_with(".zip")
            && default_name.len() == "anycon-export-alpha-box-2026-10-31.zip".len(),
        "{default_name}"
    );
    assert!(r.stdout.starts_with("exported 1 session (") && r.stdout.contains(&default_name), "{}", r.stdout);
    assert!(r.stderr.contains("1 archived session left out; add --all"), "{}", r.stderr);
    // 同一个名字再导出：拒绝覆盖；--force 才行
    let r = anycon_in(&a, Some(&cwd), &["export"], None, &[]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("error: exists:") && r.stderr.contains("already exists"), "{}", r.stderr);
    let r = anycon_in(&a, Some(&cwd), &["export", "--force", "--all"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("exported 2 sessions ("), "{}", r.stdout);
    assert!(!r.stderr.contains("left out"), "{}", r.stderr);
    // --out 给目录：默认文件名放进去；相对路径按当前目录解析
    let sub = cwd.join("backups");
    std::fs::create_dir_all(&sub).unwrap();
    let r = anycon_in(&a, Some(&cwd), &["export", "--out", "backups", "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let j = r.json();
    assert_eq!(j["sessions"], 1);
    assert!(j["path"].as_str().unwrap().starts_with(sub.canonicalize().unwrap().to_str().unwrap()), "{j}");
    assert_eq!(std::fs::read_dir(&sub).unwrap().count(), 1);

    // 筛选
    let zip = d.path().join("demo.zip");
    let zip_s = zip.to_str().unwrap();
    let r = a!("export", "--out", zip_s, "--project", "demo", "--all", "--json").await;
    assert_eq!((r.code, r.json()["sessions"].clone()), (0, serde_json::json!(1)), "{}", r.stderr);
    let r = a!("export", "--out", zip_s, "--force", "--session", &scratch_id[..12], "--json").await;
    assert_eq!(
        (r.code, r.json()["sessions"].clone()),
        (0, serde_json::json!(1)),
        "an archived session named explicitly: {}",
        r.stderr
    );
    let r = a!("export", "--out", zip_s, "--force", "--session", "last", "--json").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let r = a!("export", "--out", zip_s, "--force", "--all", "--since", "1d", "--json").await;
    assert_eq!(r.json()["sessions"], 2);
    let r = a!("export", "--out", zip_s, "--force", "--all", "--since", "2099-01-01", "--json").await;
    assert_eq!(r.json()["sessions"], 0);
    assert_eq!(a!("export", "--since", "yesterday").await.code, 2);
    assert_eq!(a!("export", "--session", "sess_nosuchsession").await.code, 1);
    assert_eq!(a!("--node", "elsewhere", "export").await.code, 2, "export is local-only");

    // 完整导出，导入到另一台 daemon
    let full = d.path().join("full.zip");
    let full_s = full.to_str().unwrap();
    assert_eq!(a!("export", "--out", full_s, "--all").await.code, 0);
    let (b, daemon_b) = node(d.path(), "beta box", "b").await;
    macro_rules! b {
        ($($x:expr),* $(,)?) => { anycon(&b, &[$($x),*], None, &[]) };
    }
    let r = b!("import", full_s, "--dry-run").await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("dry run, nothing was written: 2 sessions can be imported"), "{}", r.stdout);
    assert!(r.stdout.contains("from an export of alpha box made"), "{}", r.stdout);
    assert!(r.stderr.contains("projects that do not exist on this node: demo"), "{}", r.stderr);
    assert_eq!(
        b!("ls", "--all", "--json").await.json()["sessions"].as_array().unwrap().len(),
        0,
        "a dry run imports nothing"
    );
    let r = b!("import", full_s).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("imported 2 sessions ("), "{}", r.stdout);
    assert!(r.stdout.contains("skipped 0, failed 0"), "{}", r.stdout);
    // 导进来的会话：在列表里，历史能看，搜得到，项目不在本机也照样列出
    let ls = b!("ls", "--all", "--json").await.json();
    let ids: Vec<&str> = ls["sessions"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&demo_id.as_str()) && ids.contains(&scratch_id.as_str()), "{ids:?}");
    let h = b!("history", demo_id.as_str()).await;
    assert!(h.stdout.contains("> say hello to the zebra"), "{}", h.stdout);
    let s = b!("search", "zebra").await;
    assert!(s.stdout.contains("zebra"), "{}", s.stdout);
    let s = b!("search", "giraffe", "--json").await.json();
    assert_eq!(s["hits"][0]["session_id"], scratch_id.as_str());
    // 再导入一次：全部跳过，退出码 0
    let r = b!("import", full_s).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("imported 0 sessions (0 events, 0 attachments), skipped 2, failed 0"), "{}", r.stdout);
    assert!(r.stdout.contains(&format!("skipped {demo_id}: already exists, left as it is")), "{}", r.stdout);
    let j = b!("import", full_s, "--json").await.json();
    assert_eq!((j["imported"].as_array().unwrap().len(), j["skipped"].as_array().unwrap().len()), (0, 2));
    // 审计里是 export / import，只有数量和文件名
    let au = a!("audit", "--json").await.json();
    let e = au["entries"].as_array().unwrap().iter().find(|e| e["action"] == "export").expect("export audited");
    assert!(e["target"].as_str().unwrap().ends_with(".zip") && !e["target"].as_str().unwrap().contains('/'), "{e}");
    let au = b!("audit", "--json").await.json();
    assert!(au["entries"].as_array().unwrap().iter().any(|e| e["action"] == "import"));

    // 坏文件：不存在 / 不是 zip / 版本不认识 → 2，什么都不会导入
    assert_eq!(b!("import", "no-such-file.zip").await.code, 2);
    let junk = d.path().join("junk.zip");
    std::fs::write(&junk, b"not a zip").unwrap();
    let r = b!("import", junk.to_str().unwrap()).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("error: invalid:") && r.stderr.contains("zip"), "{}", r.stderr);
    let (c, daemon_c) = node(d.path(), "gamma box", "c").await;
    let v2 = d.path().join("v2.zip");
    rewrite(&full, &v2, |name, body| {
        if name == "manifest.json" {
            let mut m: serde_json::Value = serde_json::from_slice(&body).unwrap();
            m["version"] = serde_json::json!(2);
            return serde_json::to_vec(&m).unwrap();
        }
        body
    });
    let r = anycon(&c, &["import", v2.to_str().unwrap()], None, &[]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("format version 2"), "{}", r.stderr);
    // 一个会话里的事件损坏：这个会话失败（退出码 1、报告里列出），别的照常导入
    let broken = d.path().join("broken.zip");
    rewrite(&full, &broken, |name, mut body| {
        if name == format!("sessions/{scratch_id}/events.jsonl") {
            body.extend_from_slice(b"{ this is not an event\n");
        }
        body
    });
    let r = anycon(&c, &["import", broken.to_str().unwrap()], None, &[]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stdout.starts_with("imported 1 session ("), "{}", r.stdout);
    assert!(r.stdout.contains(&format!("failed  {scratch_id}: ")), "{}", r.stdout);
    assert!(r.stderr.contains("error: import_failed: 1 session could not be imported"), "{}", r.stderr);
    let ls = anycon(&c, &["ls", "--all", "--json"], None, &[]).await.json();
    assert_eq!(ls["sessions"].as_array().unwrap().len(), 1, "only the good session was imported");

    daemon_a.stop().await;
    daemon_b.stop().await;
    daemon_c.stop().await;
}

/// 把 `src` 的每个条目过一遍 `edit`，写成 `dst`。
fn rewrite(src: &Path, dst: &Path, mut edit: impl FnMut(&str, Vec<u8>) -> Vec<u8>) {
    let mut z = ZipWriter::new(std::fs::File::create(dst).unwrap());
    for (name, body) in zip_entries(src) {
        let body = edit(&name, body);
        z.start_file(&name, SimpleFileOptions::default()).unwrap();
        z.write_all(&body).unwrap();
    }
    z.finish().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_and_import_say_what_is_wrong_when_the_daemon_is_not_running() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    // 没初始化：4
    let r = anycon(&home, &["export"], None, &[]).await;
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert!(r.stderr.contains("error: not_initialized:"), "{}", r.stderr);
    init(&home, &d.path().join("ws"), "box", None).await;
    // 已初始化但 daemon 没跑：3，并且说明为什么要它
    for args in [&["export"][..], &["export", "--all", "--out", "x.zip"], &["import", "x.zip"]] {
        if args[0] == "import" {
            std::fs::write(d.path().join("x.zip"), b"x").unwrap();
        }
        let r = anycon_in(&home, Some(d.path()), args, None, &[]).await;
        assert_eq!(r.code, 3, "{args:?}: {}", r.stderr);
        assert!(r.stderr.starts_with("error: offline: the daemon is not running"), "{}", r.stderr);
        assert!(r.stderr.contains("anycon service start") && r.stderr.contains("owns the database"), "{}", r.stderr);
    }
    assert!(
        !d.path().join("x.zip").exists() || std::fs::read(d.path().join("x.zip")).unwrap() == b"x",
        "nothing was written"
    );
}
