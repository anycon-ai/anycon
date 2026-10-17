//! `session.git_status` / `session.diff` / `session.sync_base`（daemon.md § 10.1）：真实 `git`，「远端」是本地裸仓库，
//! `gh` 是 PATH 里的假脚本。

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anycon_protocol::*;
use common::wt::*;
use serde_json::json;

async fn status(f: &mut Fx, sid: &str) -> SessionGitStatus {
    f.call(methods::SESSION_GIT_STATUS, json!({ "session_id": sid })).await.unwrap()
}

async fn diff(f: &mut Fx, sid: &str, against: &str, path: Option<&str>) -> Result<SessionDiffResult, RpcError> {
    let mut p = json!({ "session_id": sid, "against": against });
    if let Some(path) = path {
        p["path"] = json!(path);
    }
    f.call(methods::SESSION_DIFF, p).await
}

async fn pr(f: &mut Fx, sid: String) -> SessionGitStatus {
    f.call(methods::SESSION_GIT_STATUS, json!({ "session_id": sid, "include_pr": true })).await.unwrap()
}

async fn sync(f: &mut Fx, sid: &str, abort: bool) -> Result<SessionSyncBaseResult, RpcError> {
    f.call(methods::SESSION_SYNC_BASE, json!({ "session_id": sid, "abort": abort })).await
}

/// 一个本地裸仓库当 `origin`，`main` 已经推上去。
fn add_remote(f: &Fx) -> std::path::PathBuf {
    let remote = f.td.dir.path().join("remote.git");
    f.td.git(f.td.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
    remote
}

#[tokio::test]
async fn git_status_reports_the_branch_the_base_and_the_counts() {
    let mut f = Fx::new().await;
    let s = f.ready("status please").await;
    let dir = f.dir_of(&s);
    let st = status(&mut f, &s.id).await;
    let wt = s.worktree.clone().unwrap();
    assert_eq!(st.branch.as_deref(), Some("anycon/status-please"));
    assert_eq!(st.head_sha, wt.base_sha);
    assert_eq!((st.base.as_deref(), st.base_sha.as_deref()), (Some("main"), Some(wt.base_sha.as_str())));
    assert_eq!((st.ahead, st.behind), (Some(0), Some(0)));
    assert_eq!((st.uncommitted, st.conflicts, st.merging), (0, 0, false));
    assert_eq!((st.upstream.clone(), st.unpushed, st.pr.clone(), st.pr_error.clone()), (None, None, None, None));
    // 改一个已跟踪的、加一个未跟踪的：2 个未提交；提交一个：领先 1 个
    std::fs::write(dir.join("README.md"), "# changed\n").unwrap();
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();
    assert_eq!(status(&mut f, &s.id).await.uncommitted, 2);
    f.td.git(&dir, &["add", "new.txt"]);
    f.td.git(&dir, &["commit", "-q", "-m", "add new"]);
    let st = status(&mut f, &s.id).await;
    assert_eq!((st.ahead, st.behind, st.uncommitted), (Some(1), Some(0), 1));
    // base 前进了：落后 1 个
    f.td.commit_file(&f.repo, "main-only.txt", "m", "main moved on");
    assert_eq!(status(&mut f, &s.id).await.behind, Some(1));
    // base 分支被删了：退回创建时的提交，不报错
    f.td.git(&f.repo, &["checkout", "-q", "--detach"]);
    f.td.git(&f.repo, &["branch", "-q", "-D", "main"]);
    let st = status(&mut f, &s.id).await;
    assert_eq!(st.base_sha.as_deref(), Some(wt.base_sha.as_str()));
    assert_eq!(st.behind, Some(0));
    f.td.shutdown().await;
}

#[tokio::test]
async fn git_status_knows_the_upstream_and_what_is_not_pushed() {
    let mut f = Fx::new().await;
    add_remote(&f);
    let s = f.ready("push me").await;
    let dir = f.dir_of(&s);
    f.td.git(&dir, &["push", "-q", "-u", "origin", "HEAD"]);
    let st = status(&mut f, &s.id).await;
    assert_eq!(st.upstream.as_deref(), Some("origin/anycon/push-me"));
    assert_eq!(st.unpushed, Some(0));
    f.td.commit_file(&dir, "local.txt", "x", "not pushed yet");
    assert_eq!(status(&mut f, &s.id).await.unpushed, Some(1));
    f.td.shutdown().await;
}

#[tokio::test]
async fn git_status_on_a_local_session_a_scratch_session_and_a_plain_directory() {
    let mut f = Fx::new().await;
    // 在项目目录里工作的会话：没有 base 相关字段
    let local = f.create(Some("proj"), Some("local one"), None).await.unwrap();
    f.wait_status(&local.id, SessionStatus::Idle).await;
    let st = status(&mut f, &local.id).await;
    assert_eq!(st.branch.as_deref(), Some("main"));
    assert_eq!((st.base.clone(), st.base_sha.clone(), st.ahead, st.behind), (None, None, None, None));
    std::fs::write(f.repo.join("scratch.txt"), "x").unwrap();
    assert_eq!(status(&mut f, &local.id).await.uncommitted, 1);
    // scratch 会话、不是仓库的项目
    let scratch = f.create(None, Some("scratch"), None).await.unwrap();
    let e =
        f.call::<SessionGitStatus>(methods::SESSION_GIT_STATUS, json!({ "session_id": scratch.id })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("not_git")));
    std::fs::create_dir_all(f.td.workspace.join("plain")).unwrap();
    let plain = f.create(Some("plain"), Some("p"), None).await.unwrap();
    let e =
        f.call::<SessionGitStatus>(methods::SESSION_GIT_STATUS, json!({ "session_id": plain.id })).await.unwrap_err();
    assert_eq!(reason(&e), Some("not_git"));
    // 没这个会话
    let e = f
        .call::<SessionGitStatus>(methods::SESSION_GIT_STATUS, json!({ "session_id": "sess_nope" }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    f.td.shutdown().await;
}

fn pr_json(state: &str) -> String {
    json!({
        "number": 42, "url": "https://example.test/o/r/pull/42", "title": "Add the thing", "state": state,
        "isDraft": false, "baseRefName": "main",
        "statusCheckRollup": [
            { "__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS" },
            { "__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE" },
            { "__typename": "StatusContext", "state": "PENDING" }
        ]
    })
    .to_string()
}

#[tokio::test]
async fn include_pr_uses_gh_and_classifies_why_it_cannot() {
    let mut f = Fx::new().await;
    add_remote(&f);
    let s = f.ready("pr state").await;
    // 不要 PR 时不调 gh
    status(&mut f, &s.id).await;
    assert_eq!(f.td.gh_calls(), 0);
    f.td.set_gh(&pr_json("OPEN"), "", 0);
    let st = pr(&mut f, s.id.clone()).await;
    let p = st.pr.expect("the PR is reported");
    assert_eq!((p.number, p.state, p.base_branch.as_str()), (42, PullRequestState::Open, "main"));
    assert_eq!((p.checks_passed, p.checks_failed, p.checks_pending), (1, 1, 1));
    assert_eq!(st.pr_error, None);
    // gh 被问的是这条分支，分支名在 `--` 之后（不会被当成选项）
    let calls = std::fs::read_to_string(f.td.dir.path().join("bin/gh.calls")).unwrap();
    assert!(calls.contains("pr view --json ") && calls.trim_end().ends_with("-- anycon/pr-state"), "{calls}");
    f.td.set_gh(&pr_json("MERGED"), "", 0);
    assert_eq!(pr(&mut f, s.id.clone()).await.pr.unwrap().state, PullRequestState::Merged);
    // 各种失败
    for (out, err, exit, want) in [
        ("", "no pull requests found for branch \"anycon/pr-state\"", 1, "no_pr"),
        ("", "To get started with GitHub CLI, please run:  gh auth login", 4, "gh_not_authenticated"),
        ("", "boom", 1, "error"),
        ("not json", "", 0, "error"),
    ] {
        f.td.set_gh(out, err, exit);
        let st = pr(&mut f, s.id.clone()).await;
        assert_eq!((st.pr.is_none(), st.pr_error.as_deref()), (true, Some(want)), "{err}");
    }
    f.td.remove_gh();
    assert_eq!(pr(&mut f, s.id.clone()).await.pr_error.as_deref(), Some("gh_not_installed"));
    // 没有远端
    f.td.git(&f.repo, &["remote", "remove", "origin"]);
    assert_eq!(pr(&mut f, s.id.clone()).await.pr_error.as_deref(), Some("no_remote"));
    f.td.shutdown().await;
}

#[tokio::test]
async fn diff_lists_changes_against_the_base_and_against_head_including_untracked_files() {
    let mut f = Fx::new().await;
    f.td.commit_file(&f.repo, "src/lib.rs", "fn a() {}\nfn b() {}\n", "add lib");
    f.td.commit_file(&f.repo, "gone.txt", "bye\n", "add gone");
    f.td.commit_file(&f.repo, "old name.txt", "line one\nline two\nline three\nline four\n", "to rename");
    let s = f.ready("diff things").await;
    let dir = f.dir_of(&s);
    // 已提交的改动：改 / 加 / 删 / 改名
    std::fs::write(dir.join("src/lib.rs"), "fn a() {}\nfn b() { 1 }\nfn c() {}\n").unwrap();
    std::fs::write(dir.join("added.txt"), "one\ntwo\n").unwrap();
    f.td.git(&dir, &["rm", "-q", "gone.txt"]);
    f.td.git(&dir, &["mv", "old name.txt", "new name.txt"]);
    f.td.git(&dir, &["add", "-A"]);
    f.td.git(&dir, &["commit", "-q", "-m", "work"]);
    // 未提交的改动：已暂存的、未暂存的、未跟踪的（文本 / 二进制 / 符号链接）
    std::fs::write(dir.join("README.md"), "# readme\nmore\n").unwrap();
    std::fs::write(dir.join("untracked.txt"), "a\nb\nc").unwrap();
    std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3, 0]).unwrap();
    std::os::unix::fs::symlink("README.md", dir.join("link")).unwrap();
    // 起点：base 和 HEAD 的 merge-base。main 前进不影响（merge-base 还是创建时的提交）
    f.td.commit_file(&f.repo, "main-moved.txt", "m", "main moved");
    let d = diff(&mut f, &s.id, "base", None).await.unwrap();
    assert_eq!(d.from_sha, s.worktree.as_ref().unwrap().base_sha);
    assert!(!d.truncated);
    let by = |name: &str| {
        d.files.iter().find(|x| x.path == name).unwrap_or_else(|| panic!("{name} missing: {:?}", d.files)).clone()
    };
    let paths: Vec<&str> = d.files.iter().map(|x| x.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "files are sorted by path");
    assert!(!paths.contains(&"main-moved.txt"));
    let lib = by("src/lib.rs");
    assert_eq!(
        (lib.status, lib.additions, lib.deletions, lib.untracked, lib.binary),
        (DiffFileStatus::Modified, 2, 1, false, false)
    );
    let added = by("added.txt");
    assert_eq!((added.status, added.additions, added.untracked), (DiffFileStatus::Added, 2, false));
    assert_eq!(by("gone.txt").status, DiffFileStatus::Deleted);
    let renamed = by("new name.txt");
    assert_eq!((renamed.status, renamed.old_path.as_deref()), (DiffFileStatus::Renamed, Some("old name.txt")));
    let readme = by("README.md");
    assert_eq!((readme.status, readme.additions), (DiffFileStatus::Modified, 1), "working tree changes are included");
    let un = by("untracked.txt");
    assert_eq!(
        (un.status, un.untracked, un.additions, un.binary),
        (DiffFileStatus::Added, true, 3, false),
        "no trailing newline still counts the last line"
    );
    assert!(by("blob.bin").binary && by("blob.bin").untracked);
    assert!(by("link").untracked);
    assert!(d.files.iter().all(|x| x.patch.is_none()), "no patch without a path");
    // 只看未提交的：从 HEAD 起，已提交的不出现
    let u = diff(&mut f, &s.id, "uncommitted", None).await.unwrap();
    assert_eq!(u.from_sha, f.td.git(&dir, &["rev-parse", "HEAD"]));
    let up: Vec<&str> = u.files.iter().map(|x| x.path.as_str()).collect();
    assert_eq!(up, vec!["README.md", "blob.bin", "link", "untracked.txt"], "{up:?}");
    // 缺省 against 是 base
    let dflt: SessionDiffResult = f.call(methods::SESSION_DIFF, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(dflt.files.len(), d.files.len());
    f.td.shutdown().await;
}

#[tokio::test]
async fn diff_of_one_file_gives_the_unified_diff_and_only_for_listed_paths() {
    let mut f = Fx::new().await;
    f.td.commit_file(&f.repo, "old name.txt", "line one\nline two\nline three\nline four\n", "to rename");
    let s = f.ready("one file").await;
    let dir = f.dir_of(&s);
    std::fs::write(dir.join("README.md"), "# readme\nadded line\n").unwrap();
    std::fs::write(dir.join("fresh.txt"), "x\ny\n").unwrap();
    std::fs::write(dir.join("fresh.bin"), [0u8; 16]).unwrap();
    std::fs::write(dir.join("empty.txt"), "").unwrap();
    f.td.git(&dir, &["mv", "old name.txt", "renamed.txt"]);
    let d = diff(&mut f, &s.id, "base", Some("README.md")).await.unwrap();
    assert_eq!(d.files.len(), 1);
    let p = d.files[0].patch.clone().unwrap();
    assert!(p.starts_with("diff --git a/README.md b/README.md"), "{p}");
    assert!(p.contains("+added line") && p.contains("@@"), "{p}");
    assert!(!d.files[0].patch_truncated);
    // 未跟踪：daemon 自己拼 new file 的 patch
    let d = diff(&mut f, &s.id, "base", Some("fresh.txt")).await.unwrap();
    assert_eq!(
        d.files[0].patch.as_deref().unwrap(),
        "diff --git a/fresh.txt b/fresh.txt\nnew file mode 100644\n--- /dev/null\n+++ b/fresh.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n"
    );
    let d = diff(&mut f, &s.id, "base", Some("fresh.bin")).await.unwrap();
    assert!(d.files[0].patch.as_deref().unwrap().contains("Binary files /dev/null and b/fresh.bin differ"));
    let d = diff(&mut f, &s.id, "base", Some("empty.txt")).await.unwrap();
    assert!(d.files[0].patch.as_deref().unwrap().starts_with("diff --git a/empty.txt b/empty.txt\nnew file mode"));
    // 改名：带上旧路径，git 认得出是改名
    let d = diff(&mut f, &s.id, "base", Some("renamed.txt")).await.unwrap();
    assert!(d.files[0].patch.as_deref().unwrap().contains("rename from old name.txt"), "{:?}", d.files[0].patch);
    // 不在列表里的路径：一律 not_found（包括 `..`、绝对路径、被忽略的文件、项目外的文件）
    std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.join("ignored.txt"), "i").unwrap();
    for bad in ["nope.txt", "../README.md", "/etc/passwd", "ignored.txt", "src", ".git/config", "README.md/"] {
        let e = diff(&mut f, &s.id, "base", Some(bad)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound, "{bad}");
    }
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_big_patch_is_cut_at_256_kib() {
    let mut f = Fx::new().await;
    let s = f.ready("big patch").await;
    let dir = f.dir_of(&s);
    let big: String = (0..40_000).map(|i| format!("line number {i}\n")).collect();
    std::fs::write(dir.join("big.txt"), &big).unwrap();
    f.td.git(&dir, &["add", "big.txt"]);
    let d = diff(&mut f, &s.id, "base", Some("big.txt")).await.unwrap();
    let f0 = &d.files[0];
    assert!(f0.patch_truncated);
    assert!(f0.patch.as_ref().unwrap().len() <= 256 * 1024, "{}", f0.patch.as_ref().unwrap().len());
    // 未跟踪的大文件：patch 同样截断
    std::fs::write(dir.join("big2.txt"), &big).unwrap();
    let d = diff(&mut f, &s.id, "base", Some("big2.txt")).await.unwrap();
    assert!(d.files[0].patch_truncated);
    assert!(d.files[0].patch.as_ref().unwrap().len() <= 256 * 1024);
    // 超过 1 MiB 的未跟踪文件不数行数，按二进制处理
    std::fs::write(dir.join("huge.txt"), "x\n".repeat(700_000)).unwrap();
    let d = diff(&mut f, &s.id, "base", None).await.unwrap();
    let h = d.files.iter().find(|x| x.path == "huge.txt").unwrap();
    assert!(h.binary && h.additions == 0);
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_file_list_is_capped_at_a_thousand() {
    let mut f = Fx::new().await;
    let s = f.ready("many files").await;
    let dir = f.dir_of(&s);
    std::fs::create_dir_all(dir.join("many")).unwrap();
    for i in 0..1100 {
        std::fs::write(dir.join(format!("many/f{i:04}.txt")), "x\n").unwrap();
    }
    let d = diff(&mut f, &s.id, "uncommitted", None).await.unwrap();
    assert_eq!(d.files.len(), 1000);
    assert!(d.truncated);
    f.td.shutdown().await;
}

#[tokio::test]
async fn diff_needs_a_worktree_for_the_base_view_and_never_runs_repository_configured_programs() {
    let mut f = Fx::new().await;
    let local = f.create(Some("proj"), Some("local"), None).await.unwrap();
    f.wait_status(&local.id, SessionStatus::Idle).await;
    let e = diff(&mut f, &local.id, "base", None).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("not_worktree")));
    std::fs::write(f.repo.join("n.txt"), "n\n").unwrap();
    assert_eq!(diff(&mut f, &local.id, "uncommitted", None).await.unwrap().files.len(), 1);
    // 仓库配置里的外部 diff / textconv 不能被执行
    let marker = f.td.dir.path().join("EXECUTED");
    let script = f.td.dir.path().join("evil.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nPATH=/usr/bin:/bin\ntouch {}\ncat \"$1\" 2>/dev/null\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // textconv 只对**已跟踪**的文件起作用（未跟踪的文件的 patch 是 daemon 自己拼的，git 根本不会去转换它）：
    // 先提交一个 `*.txt` 再改它，去掉 `--no-textconv` 这里就红
    f.td.commit_file(&f.repo, "tracked.txt", "before\n", "add a tracked text file");
    f.td.git(&f.repo, &["config", "diff.external", script.to_str().unwrap()]);
    f.td.git(&f.repo, &["config", "diff.evil.textconv", script.to_str().unwrap()]);
    std::fs::write(f.repo.join(".gitattributes"), "*.txt diff=evil\n").unwrap();
    std::fs::write(f.repo.join("README.md"), "# changed\n").unwrap();
    std::fs::write(f.repo.join("n.txt"), "changed\n").unwrap();
    std::fs::write(f.repo.join("tracked.txt"), "after\n").unwrap();
    let d = diff(&mut f, &local.id, "uncommitted", Some("README.md")).await.unwrap();
    assert!(d.files[0].patch.as_deref().unwrap().contains("+# changed"));
    let d = diff(&mut f, &local.id, "uncommitted", Some("n.txt")).await.unwrap();
    assert!(d.files[0].patch.is_some());
    let d = diff(&mut f, &local.id, "uncommitted", Some("tracked.txt")).await.unwrap();
    let patch = d.files[0].patch.as_deref().unwrap();
    assert!(patch.contains("-before") && patch.contains("+after"), "the raw content, not a converted one: {patch}");
    // 列表（numstat）也不经过 textconv
    let list = diff(&mut f, &local.id, "uncommitted", None).await.unwrap();
    let t = list.files.iter().find(|x| x.path == "tracked.txt").unwrap();
    assert_eq!((t.additions, t.deletions), (1, 1));
    assert!(!marker.exists(), "the configured diff program must not run");
    f.td.shutdown().await;
}

#[tokio::test]
async fn sync_base_reports_up_to_date_then_merges_local_base_commits() {
    let mut f = Fx::new().await;
    let s = f.ready("sync me").await;
    let dir = f.dir_of(&s);
    let r = sync(&mut f, &s.id, false).await.unwrap();
    assert_eq!((r.outcome, r.commits), (SyncOutcome::UpToDate, 0));
    f.td.commit_file(&f.repo, "one.txt", "1", "base commit one");
    f.td.commit_file(&f.repo, "two.txt", "2", "base commit two");
    f.td.commit_file(&dir, "mine.txt", "m", "my own commit");
    let r = sync(&mut f, &s.id, false).await.unwrap();
    assert_eq!((r.outcome, r.commits, r.conflicts.len()), (SyncOutcome::Merged, 2, 0));
    assert!(dir.join("one.txt").is_file() && dir.join("two.txt").is_file() && dir.join("mine.txt").is_file());
    let st = status(&mut f, &s.id).await;
    assert_eq!((st.behind, st.merging), (Some(0), false));
    // 事件：up_to_date、merged（带提交数和 base）
    let evs = f.git_events(&s.id).await;
    let sync_evs: Vec<(GitAction, Option<u32>, Option<String>)> = evs
        .iter()
        .filter_map(|e| match e {
            Event::Git { action, count, base, .. } if format!("{action:?}").starts_with("Sync") => {
                Some((*action, *count, base.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        sync_evs,
        vec![
            (GitAction::SyncUpToDate, None, Some("main".into())),
            (GitAction::SyncMerged, Some(2), Some("main".into()))
        ]
    );
    // 审计
    let audit: NodeAuditResult = f.call(methods::NODE_AUDIT, json!({ "limit": 100 })).await.unwrap();
    let a: Vec<_> = audit.entries.iter().filter(|e| e.action == "session.sync_base").collect();
    assert_eq!(a.len(), 2);
    assert!(a.iter().any(|e| e.detail["outcome"] == "merged" && e.detail["commits"] == 2));
    f.td.shutdown().await;
}

#[tokio::test]
async fn sync_base_fetches_a_remote_tracking_base_first() {
    let mut f = Fx::new().await;
    let remote = add_remote(&f);
    // 另一个人往远端推了新提交；本地的 origin/main 还是旧的
    let other = f.td.dir.path().join("other");
    f.td.git(f.td.dir.path(), &["clone", "-q", remote.to_str().unwrap(), other.to_str().unwrap()]);
    let s = f
        .create(Some("proj"), Some("tracked base"), Some(json!({ "mode": "worktree", "base": "origin/main" })))
        .await
        .unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    f.td.commit_file(&other, "remote.txt", "r", "remote commit");
    f.td.git(&other, &["push", "-q", "origin", "main"]);
    let before = f.td.git(&f.repo, &["rev-parse", "origin/main"]);
    let r = sync(&mut f, &s.id, false).await.unwrap();
    assert_eq!((r.outcome, r.commits), (SyncOutcome::Merged, 1), "{r:?}");
    assert_ne!(f.td.git(&f.repo, &["rev-parse", "origin/main"]), before, "origin/main was fetched");
    assert!(f.dir_of(&s).join("remote.txt").is_file());
    // 本地分支作 base 时不 fetch
    let s2 =
        f.create(Some("proj"), Some("local base"), Some(json!({ "mode": "worktree", "base": "main" }))).await.unwrap();
    f.wait_wt_state(&s2.id, WorktreeState::Active).await;
    f.wait_status(&s2.id, SessionStatus::Idle).await;
    f.td.commit_file(&other, "remote2.txt", "r2", "another remote commit");
    f.td.git(&other, &["push", "-q", "origin", "main"]);
    let r = sync(&mut f, &s2.id, false).await.unwrap();
    assert_eq!(r.outcome, SyncOutcome::UpToDate, "a local base is merged as it is now, nothing is fetched");
    // fetch 失败：远端没了
    let s3 = f
        .create(Some("proj"), Some("gone remote"), Some(json!({ "mode": "worktree", "base": "origin/main" })))
        .await
        .unwrap();
    f.wait_wt_state(&s3.id, WorktreeState::Active).await;
    f.wait_status(&s3.id, SessionStatus::Idle).await;
    std::fs::rename(&remote, f.td.dir.path().join("moved.git")).unwrap();
    let e = sync(&mut f, &s3.id, false).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Internal, Some("fetch_failed")), "{e:?}");
    f.td.shutdown().await;
}

fn conflicting_session_setup(f: &Fx) {
    f.td.commit_file(&f.repo, "shared.txt", "original\n", "shared file");
}

#[tokio::test]
async fn sync_base_stops_in_the_middle_on_conflicts_and_can_be_aborted() {
    let mut f = Fx::new().await;
    conflicting_session_setup(&f);
    let s = f.ready("conflict").await;
    let dir = f.dir_of(&s);
    f.td.commit_file(&f.repo, "shared.txt", "from main\n", "main edits it");
    f.td.commit_file(&dir, "shared.txt", "from the branch\n", "branch edits it");
    let r = sync(&mut f, &s.id, false).await.unwrap();
    assert_eq!(r.outcome, SyncOutcome::Conflicts);
    assert_eq!(r.conflicts, vec!["shared.txt".to_string()]);
    let st = status(&mut f, &s.id).await;
    assert!(st.merging);
    assert_eq!((st.conflicts, st.uncommitted), (1, 1));
    assert!(std::fs::read_to_string(dir.join("shared.txt")).unwrap().contains("<<<<<<<"));
    // 事件带冲突文件
    let evs = f.git_events(&s.id).await;
    assert!(evs.iter().any(|e| matches!(e, Event::Git { action: GitAction::SyncConflicts, files, .. } if files == &vec!["shared.txt".to_string()])));
    // 合并还停在中途：再同步 → invalid（merge_in_progress）
    let e = sync(&mut f, &s.id, false).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("merge_in_progress")));
    // 放弃
    let r = sync(&mut f, &s.id, true).await.unwrap();
    assert_eq!((r.outcome, r.commits), (SyncOutcome::Aborted, 0));
    let st = status(&mut f, &s.id).await;
    assert_eq!((st.merging, st.uncommitted), (false, 0));
    assert_eq!(std::fs::read_to_string(dir.join("shared.txt")).unwrap(), "from the branch\n");
    assert!(f.git_actions(&s.id).await.contains(&GitAction::SyncAborted));
    // 没有合并可放弃
    let e = sync(&mut f, &s.id, true).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    f.td.shutdown().await;
}

#[tokio::test]
async fn sync_base_refuses_uncommitted_changes_and_busy_sessions_and_blocks_sends_meanwhile() {
    let mut f = Fx::new().await;
    let s = f.ready("busy things").await;
    let dir = f.dir_of(&s);
    f.td.commit_file(&f.repo, "x.txt", "x", "base moved");
    // 未提交（含未跟踪）
    std::fs::write(dir.join("untracked.txt"), "u").unwrap();
    let e = sync(&mut f, &s.id, false).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("uncommitted_changes")));
    std::fs::remove_file(dir.join("untracked.txt")).unwrap();
    // turn 在跑
    f.send(&s.id, "hold on please").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Running).await;
    let e = sync(&mut f, &s.id, false).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    f.td.fake.last_handle().release();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    // 同步期间会话标记为忙：send、归档、再同步都是 busy；结束之后清除
    f.td.ctx.worktrees.hooks.sync_delay_ms.store(600, Ordering::SeqCst);
    let (rpc_td, sid) = (f.td.rpc(), s.id.clone());
    let other = f.td.client("syncer");
    let handle = other.handle.clone();
    let slow = tokio::spawn(async move {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: RpcId::Num(77),
            method: methods::SESSION_SYNC_BASE.into(),
            params: json!({ "session_id": sid }),
        };
        rpc_td.handle_request(&handle, req).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let e = f.send(&s.id, "while syncing").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy, "send is busy while the base is being merged");
    let e = f.call::<Empty>(methods::SESSION_ARCHIVE, json!({ "session_id": s.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    let e = sync(&mut f, &s.id, false).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    let resp = slow.await.unwrap();
    assert!(matches!(&resp, RpcMessage::Response(r) if r.error.is_none()), "{resp:?}");
    f.send(&s.id, "after syncing").await.unwrap();
    f.td.shutdown().await;
}

#[tokio::test]
async fn sync_base_with_git_failing_for_another_reason_cleans_up_and_reports_the_output() {
    let mut f = Fx::new().await;
    let s = f.ready("no identity").await;
    let dir = f.dir_of(&s);
    f.td.commit_file(&f.repo, "x.txt", "x", "base moved");
    f.td.commit_file(&dir, "y.txt", "y", "branch moved");
    // 提交身份不可用：merge 要做合并提交，失败
    f.td.git(&dir, &["config", "user.useConfigOnly", "true"]);
    f.td.git(&dir, &["config", "user.name", ""]);
    f.td.git(&dir, &["config", "user.email", ""]);
    let e = sync(&mut f, &s.id, false).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Internal, Some("git_failed")), "{e:?}");
    assert!(e.message.len() < 2200);
    // 收拾干净了：没有停在中途的合并，也没有未提交的改动
    let st = status(&mut f, &s.id).await;
    assert_eq!((st.merging, st.uncommitted, st.conflicts), (false, 0, 0));
    f.td.shutdown().await;
}

#[tokio::test]
async fn repository_hooks_are_not_executed_by_the_daemons_git() {
    let mut f = Fx::new().await;
    let s = f.ready("hooks").await;
    let dir = f.dir_of(&s);
    f.td.commit_file(&f.repo, "x.txt", "x", "base moved");
    f.td.commit_file(&dir, "y.txt", "y", "branch moved");
    // 钩子是 shell 内建的重定向（测试 daemon 的 PATH 里没有别的程序）。测试自己的 git 不会再跑 git 了，钩子装在提交之后
    let marker = f.td.dir.path().join("HOOK-RAN");
    for hook in
        ["post-merge", "pre-merge-commit", "commit-msg", "post-checkout", "reference-transaction", "prepare-commit-msg"]
    {
        let p = f.repo.join(".git/hooks").join(hook);
        std::fs::write(&p, format!("#!/bin/sh\n: > {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let r = sync(&mut f, &s.id, false).await.unwrap();
    assert_eq!(r.outcome, SyncOutcome::Merged);
    let s2 = f.ready("hooks again").await;
    assert!(f.dir_of(&s2).join("README.md").is_file());
    assert!(!marker.exists(), "no repository hook may run when the daemon merges or creates a worktree");
    f.td.shutdown().await;
}

#[tokio::test]
async fn git_calls_work_in_the_worktree_after_the_session_was_given_a_scratch_style_cwd_check() {
    // 一个会话的 cwd 不是它的 worktree 时（目录被换掉）git_status / diff / sync 都拒绝，不在别处跑
    let mut f = Fx::new().await;
    let s = f.ready("swap me").await;
    let dir = f.dir_of(&s);
    let elsewhere = f.td.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    f.td.git(&elsewhere, &["init", "-q", "-b", "main"]);
    f.td.git(&f.repo, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
    let e = f.call::<SessionGitStatus>(methods::SESSION_GIT_STATUS, json!({ "session_id": s.id })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::NotFound, Some("worktree_missing")));
    assert_eq!(code(&diff(&mut f, &s.id, "base", None).await.unwrap_err()), ErrorCode::NotFound);
    assert_eq!(code(&sync(&mut f, &s.id, false).await.unwrap_err()), ErrorCode::NotFound);
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_mock_agents_write_shows_up_in_the_worktree_diff_and_not_in_the_project_directory() {
    use anycon_daemon::testutil::TestDaemon;
    let td = TestDaemon::start_mock_git(&[]).await;
    let repo = td.new_repo("proj");
    let mut f = Fx::from_daemon(td, repo.clone());
    let s: SessionSummary = f
        .call(
            methods::SESSION_CREATE,
            json!({ "agent": "mock", "project": "proj", "prompt": "write something", "message_id": uuid(), "workspace": { "mode": "worktree" } }),
        )
        .await
        .unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let dir = f.dir_of(&s);
    // mock 的 cwd 就是 worktree：文件落在那里，项目目录里没有
    assert!(dir.join("anycon-mock.txt").is_file());
    assert!(!repo.join("anycon-mock.txt").exists());
    let changed: Vec<String> = f
        .events(&s.id)
        .await
        .into_iter()
        .filter_map(|e| match e.event {
            Event::FileChanged { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(changed, vec!["anycon-mock.txt".to_string()], "relative to the cwd");
    let st = status(&mut f, &s.id).await;
    assert_eq!(st.uncommitted, 1);
    let d = diff(&mut f, &s.id, "base", Some("anycon-mock.txt")).await.unwrap();
    assert!(d.files[0].untracked);
    assert!(d.files[0].patch.as_deref().unwrap().contains("+mock write turn "), "{:?}", d.files[0].patch);
    // 提交之后再写一次：已跟踪文件的修改
    f.td.git(&dir, &["add", "-A"]);
    f.td.git(&dir, &["commit", "-q", "-m", "mock wrote"]);
    f.send(&s.id, "write again").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let d = diff(&mut f, &s.id, "uncommitted", None).await.unwrap();
    assert_eq!(d.files.len(), 1);
    assert_eq!(
        (d.files[0].path.as_str(), d.files[0].status, d.files[0].untracked),
        ("anycon-mock.txt", DiffFileStatus::Modified, false)
    );
    // 本分支相对 base 的改动里它是新增（一个已提交 + 一行未提交）
    let d = diff(&mut f, &s.id, "base", None).await.unwrap();
    assert_eq!((d.files[0].status, d.files[0].additions), (DiffFileStatus::Added, 2));
    f.td.shutdown().await;
}
