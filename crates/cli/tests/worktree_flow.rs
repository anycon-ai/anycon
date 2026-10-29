//! 每会话 worktree 的 CLI 端到端（docs/spec/cli.md § 3.5，ADR-34）：真实 `anycon` 进程 + 真 mock agent + 真实 `git`，不联网。
//! daemon 的环境里 `GIT_CONFIG_GLOBAL` 指向临时文件，不读开发机的 `~/.gitconfig`。

mod common;

use std::path::{Path, PathBuf};

use common::*;

struct World {
    d: tempfile::TempDir,
    home: PathBuf,
    ws: PathBuf,
    gitcfg: PathBuf,
}

impl World {
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", &self.gitcfg)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git is installed");
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(&self, dir: &Path, file: &str, content: &str, msg: &str) {
        std::fs::write(dir.join(file), content).unwrap();
        self.git(dir, &["add", "-A"]);
        self.git(dir, &["commit", "-q", "-m", msg]);
    }

    async fn a(&self, args: &[&str]) -> Run {
        anycon(&self.home, args, None, &[]).await
    }

    fn repo(&self) -> PathBuf {
        anycon_daemon::sys::canonicalize(self.ws.join("demo")).unwrap()
    }
}

async fn world() -> (World, DaemonProc) {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let ws = d.path().join("ws");
    let gitcfg = d.path().join("gitconfig");
    std::fs::write(
        &gitcfg,
        "[user]\n\tname = Anycon Test\n\temail = test@example.invalid\n[init]\n\tdefaultBranch = main\n",
    )
    .unwrap();
    init(&home, &ws, "box", None).await;
    for k in ["agents.claude-code.enabled", "agents.codex.enabled"] {
        assert_eq!(anycon(&home, &["config", "set", k, "false"], None, &[]).await.code, 0);
    }
    let daemon = start_daemon(
        &home,
        &[("ANYCON_MOCK_AGENT", "1"), ("GIT_CONFIG_GLOBAL", gitcfg.to_str().unwrap()), ("GIT_CONFIG_NOSYSTEM", "1")],
    )
    .await;
    let w = World { d, home, ws, gitcfg };
    assert_eq!(w.a(&["projects", "create", "demo", "--git"]).await.code, 0);
    let repo = w.repo();
    w.commit(&repo, "README.md", "# demo\n", "initial");
    (w, daemon)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worktree_session_from_new_to_diff_sync_archive_and_back() {
    let (w, daemon) = world().await;
    let repo = w.repo();
    // new --worktree：跟随输出里有一行 `worktree: <分支> (base <base>)`
    let r = w.a(&["new", "--agent", "mock", "--project", "demo", "--worktree", "write a file"]).await;
    assert_eq!(r.code, 0, "stderr: {}\nstdout: {}", r.stderr, r.stdout);
    assert!(r.stdout.contains("worktree: anycon/write-a-file (base main)"), "{}", r.stdout);
    assert!(repo.join(".anycon/worktrees/write-a-file/anycon-mock.txt").is_file());
    // ls 多一列 BRANCH
    let ls = w.a(&["ls"]).await;
    let header = ls.stdout.lines().next().unwrap();
    assert!(header.contains("BRANCH"), "{}", ls.stdout);
    assert!(ls.stdout.contains("anycon/write-a-file"), "{}", ls.stdout);
    let lsj = w.a(&["ls", "--json"]).await.json();
    assert_eq!(lsj["sessions"][0]["worktree"]["branch"], "anycon/write-a-file");
    assert_eq!(lsj["sessions"][0]["worktree"]["state"], "active");
    // diff --stat：一行状态 + 文件列表
    let stat = w.a(&["diff", "last", "--stat"]).await;
    assert_eq!(stat.code, 0, "{}", stat.stderr);
    assert!(stat.stdout.contains("anycon/write-a-file ← main"), "{}", stat.stdout);
    assert!(stat.stdout.contains("1 uncommitted"), "{}", stat.stdout);
    assert!(stat.stdout.contains("?  anycon-mock.txt") && stat.stdout.contains("+1 −0"), "{}", stat.stdout);
    // diff：完整的 unified diff
    let full = w.a(&["diff", "last"]).await;
    assert!(full.stdout.contains("diff --git a/anycon-mock.txt b/anycon-mock.txt"), "{}", full.stdout);
    assert!(full.stdout.contains("+mock write turn "), "{}", full.stdout);
    // 路径过滤、没有匹配的路径
    assert!(w.a(&["diff", "last", "anycon-mock.txt"]).await.stdout.contains("+mock write turn "));
    assert!(w.a(&["diff", "last", "nothing/"]).await.stdout.contains("no changes"));
    let dj = w.a(&["diff", "last", "--json"]).await.json();
    assert_eq!(dj["files"][0]["path"], "anycon-mock.txt");
    assert!(dj["files"][0]["patch"].as_str().unwrap().contains("+mock write turn "));
    let dj = w.a(&["diff", "last", "--stat", "--json"]).await.json();
    assert!(dj["files"][0]["patch"].is_null(), "no patch in the stat form");

    // 提交；base 前进；sync：合并了 1 个提交
    let wt = repo.join(".anycon/worktrees/write-a-file");
    w.git(&wt, &["add", "-A"]);
    w.git(&wt, &["commit", "-q", "-m", "mock wrote"]);
    w.commit(&repo, "base.txt", "base\n", "base moved");
    let s = w.a(&["sync", "last"]).await;
    assert_eq!(s.code, 0, "{}", s.stderr);
    assert!(s.stdout.contains("merged 1 commit(s) from main into anycon/write-a-file"), "{}", s.stdout);
    assert!(wt.join("base.txt").is_file());
    assert!(w.a(&["sync", "last"]).await.stdout.contains("already up to date with main"));
    // 冲突：退出码 1，列出文件；--abort 放弃
    w.commit(&repo, "README.md", "# from main\n", "main edits readme");
    w.commit(&wt, "README.md", "# from the branch\n", "branch edits readme");
    let c = w.a(&["sync", "last"]).await;
    assert_eq!(c.code, 1, "{}", c.stderr);
    assert!(
        c.stdout.contains("conflicts merging main into anycon/write-a-file:") && c.stdout.contains("  README.md"),
        "{}",
        c.stdout
    );
    assert!(c.stdout.contains("--abort"));
    // 合并还停在中途：再同步是 invalid（merge_in_progress）
    let again = w.a(&["sync", "last"]).await;
    assert_eq!(again.code, 2, "{}", again.stderr);
    assert!(again.stderr.contains("merge_in_progress"), "{}", again.stderr);
    let ab = w.a(&["sync", "last", "--abort"]).await;
    assert_eq!((ab.code, ab.stdout.trim()), (0, "merge aborted"), "{}", ab.stderr);
    assert_eq!(w.a(&["sync", "last", "--abort"]).await.code, 2, "nothing to abort is a usage error (invalid)");

    // worktrees 列表
    let wl = w.a(&["worktrees"]).await;
    assert!(wl.stdout.starts_with("ID"), "{}", wl.stdout);
    for col in ["PROJECT", "BRANCH", "SESSION", "STATE", "UNCOMMITTED", "AHEAD", "SIZE"] {
        assert!(wl.stdout.lines().next().unwrap().contains(col), "{col}: {}", wl.stdout);
    }
    assert!(
        wl.stdout.contains("demo") && wl.stdout.contains("anycon/write-a-file") && wl.stdout.contains("active"),
        "{}",
        wl.stdout
    );
    let wj = w.a(&["worktrees", "--json", "--project", "demo"]).await.json();
    assert_eq!(wj["worktrees"][0]["branch"], "anycon/write-a-file");

    // 归档：--remove-worktree 会丢弃未提交的改动，非交互时必须带 --yes（退出码 2），什么都没动
    let refused = w.a(&["archive", "last", "--remove-worktree"]).await;
    assert_eq!(refused.code, 2, "{}", refused.stderr);
    assert!(refused.stderr.contains("--yes"), "{}", refused.stderr);
    assert!(wt.is_dir());
    assert_eq!(w.a(&["archive", "last", "--keep-worktree", "--remove-worktree"]).await.code, 2, "contradiction");
    // `delete` 的两个会丢东西的选项都要确认：什么都没动
    for flag in ["--remove-worktree", "--delete-branch"] {
        let r = w.a(&["delete", "last", flag]).await;
        assert_eq!(r.code, 2, "{flag}: {}", r.stderr);
        assert!(r.stderr.contains(flag) && r.stderr.contains("--yes"), "{flag}: {}", r.stderr);
        assert!(wt.is_dir(), "{flag}: nothing was deleted");
    }
    let sessions = w.a(&["ls", "--json"]).await.json();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1, "the session is still there");
    // 缺省 auto：干净的删目录，分支上有自己的提交所以分支留着
    let ar = w.a(&["archive", "last"]).await;
    assert_eq!(ar.code, 0, "{}", ar.stderr);
    assert!(
        ar.stdout.contains("archived") && ar.stdout.contains("worktree anycon/write-a-file: removed"),
        "{}",
        ar.stdout
    );
    assert!(!wt.exists());
    assert!(w.git(&repo, &["branch", "--list", "anycon/write-a-file"]).contains("anycon/write-a-file"));
    // 已归档的会话不能直接发消息：退出码 2，告诉用户先 `anycon unarchive`
    let sent = w.a(&["send", "last", "more work", "--no-wait"]).await;
    assert_eq!(sent.code, 2, "{}", sent.stderr);
    assert!(sent.stderr.contains("archived") && sent.stderr.contains("anycon unarchive"), "{}", sent.stderr);
    // unarchive：按分支重建
    let un = w.a(&["unarchive", "last"]).await;
    assert_eq!(un.code, 0, "{}", un.stderr);
    assert!(un.stdout.contains("worktree anycon/write-a-file: active"), "{}", un.stdout);
    assert!(wt.join("anycon-mock.txt").is_file());
    // 又改了东西：archive --remove-worktree --yes 丢弃它们
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();
    let rm = w.a(&["archive", "last", "--remove-worktree", "--yes"]).await;
    assert_eq!(rm.code, 0, "{}", rm.stderr);
    assert!(!wt.exists());
    // delete + delete-branch：目录、分支都没了，记录也没了
    let del = w.a(&["delete", "last", "--delete-branch", "--yes"]).await;
    assert_eq!(del.code, 0, "{}", del.stderr);
    assert!(del.stdout.starts_with("deleted"), "{}", del.stdout);
    assert!(!w.git(&repo, &["branch", "--list", "anycon/write-a-file"]).contains("anycon/write-a-file"));
    assert!(w.a(&["worktrees"]).await.stdout.contains("no worktrees"));
    daemon.stop().await;
    drop(w.d);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_with_keep_leaves_a_worktree_to_clean_up_with_the_worktrees_command() {
    let (w, daemon) = world().await;
    let repo = w.repo();
    let r = w
        .a(&["new", "--agent", "mock", "--project", "demo", "--branch", "feature/mine", "--base", "main", "write one"])
        .await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("worktree: feature/mine (base main)"), "{}", r.stdout);
    let wt = repo.join(".anycon/worktrees/mine");
    assert!(wt.join("anycon-mock.txt").is_file(), "the directory is the last segment of the branch");
    let del = w.a(&["delete", "last", "--keep-worktree"]).await;
    assert_eq!(del.code, 0, "{}", del.stderr);
    assert!(del.stdout.contains("worktree feature/mine stays as .anycon/worktrees/mine (kept)"), "{}", del.stdout);
    let list = w.a(&["worktrees"]).await;
    assert!(list.stdout.contains("feature/mine") && list.stdout.contains("kept"), "{}", list.stdout);
    let id = w.a(&["worktrees", "--json"]).await.json()["worktrees"][0]["id"].as_str().unwrap().to_string();
    // 有未提交的改动：不加 --force 不删（invalid -> 2）
    let no = w.a(&["worktrees", "remove", &id[..8]]).await;
    assert_eq!(no.code, 2, "{}", no.stderr);
    assert!(no.stderr.contains("uncommitted_changes"), "{}", no.stderr);
    assert!(wt.is_dir());
    // `--force` 丢弃未提交的改动：和 `archive --remove-worktree` 一样，非交互时没有 --yes 拒绝（退出码 2），什么都没动
    let forced = w.a(&["worktrees", "remove", &id[3..10], "--force"]).await;
    assert_eq!(forced.code, 2, "{}", forced.stderr);
    assert!(forced.stderr.contains("--force") && forced.stderr.contains("--yes"), "{}", forced.stderr);
    assert!(wt.is_dir(), "nothing was removed");
    // `--delete-branch` 会丢没合并的提交：非交互时没有 --yes 一样拒绝（退出码 2），什么都没动
    let unconfirmed = w.a(&["worktrees", "remove", &id[3..10], "--force", "--delete-branch"]).await;
    assert_eq!(unconfirmed.code, 2, "{}", unconfirmed.stderr);
    assert!(
        unconfirmed.stderr.contains("--force")
            && unconfirmed.stderr.contains("--delete-branch")
            && unconfirmed.stderr.contains("--yes"),
        "{}",
        unconfirmed.stderr
    );
    assert!(wt.is_dir() && w.git(&repo, &["branch", "--list", "feature/mine"]).contains("feature/mine"));
    let yes = w.a(&["worktrees", "remove", &id[3..10], "--force", "--delete-branch", "--yes"]).await;
    assert_eq!(yes.code, 0, "{}", yes.stderr);
    assert!(
        yes.stdout.contains("removed worktree .anycon/worktrees/mine (feature/mine) and its branch"),
        "{}",
        yes.stdout
    );
    assert!(!wt.exists());
    assert!(w.git(&repo, &["branch", "--list", "feature/mine"]).is_empty());
    assert!(w.a(&["worktrees"]).await.stdout.contains("no worktrees"));
    // 没有的 id
    assert_eq!(w.a(&["worktrees", "remove", "nope"]).await.code, 1);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_working_place_follows_the_node_and_the_project() {
    let (w, daemon) = world().await;
    let repo = w.repo();
    std::fs::create_dir_all(w.ws.join("plain")).unwrap();
    // 缺省：节点的 worktree.default = worktree，仓库项目 → 在 worktree 里
    let r = w.a(&["new", "--agent", "mock", "--project", "demo", "hello default"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("worktree: anycon/hello-default (base main)"), "{}", r.stdout);
    // --here：在项目目录本身
    let here = w.a(&["new", "--agent", "mock", "--project", "demo", "--here", "hello here"]).await;
    assert_eq!(here.code, 0, "{}", here.stderr);
    assert!(!here.stdout.contains("worktree:"), "{}", here.stdout);
    assert!(!repo.join(".anycon/worktrees/hello-here").exists());
    // 节点设成 local：缺省就是项目目录
    assert_eq!(w.a(&["config", "set", "worktree.default", "local"]).await.code, 0);
    let local = w.a(&["new", "--agent", "mock", "--project", "demo", "hello local"]).await;
    assert!(!local.stdout.contains("worktree:"), "{}", local.stdout);
    // 但明确要 worktree 照样给
    let forced = w.a(&["new", "--agent", "mock", "--project", "demo", "--worktree", "hello forced"]).await;
    assert!(forced.stdout.contains("worktree: anycon/hello-forced"), "{}", forced.stdout);
    assert_eq!(w.a(&["config", "set", "worktree.default", "worktree"]).await.code, 0);
    // 不是仓库的项目：缺省退回项目目录（不报错），明确要 worktree 是参数错误（2）
    let plain = w.a(&["new", "--agent", "mock", "--project", "plain", "hello plain"]).await;
    assert_eq!(plain.code, 0, "{}", plain.stderr);
    assert!(plain.stderr.contains("working in the project directory"), "{}", plain.stderr);
    let bad = w.a(&["new", "--agent", "mock", "--project", "plain", "--worktree", "no thanks"]).await;
    assert_eq!(bad.code, 2, "{}", bad.stderr);
    assert!(bad.stderr.contains("not_worktree_ready"), "{}", bad.stderr);
    // scratch 会话不能开 worktree
    let scratch = w.a(&["new", "--agent", "mock", "--worktree", "scratch one"]).await;
    assert_eq!(scratch.code, 2, "{}", scratch.stderr);
    assert!(scratch.stderr.contains("--project"), "{}", scratch.stderr);
    let scratch = w.a(&["new", "--agent", "mock", "--branch", "x/y", "scratch two"]).await;
    assert_eq!(scratch.code, 2, "{}", scratch.stderr);
    // 不存在的 base / 已存在的分支
    let nb = w.a(&["new", "--agent", "mock", "--project", "demo", "--base", "no-such-ref", "x"]).await;
    assert_eq!(nb.code, 2, "{}", nb.stderr);
    assert!(nb.stderr.contains("bad_base"), "{}", nb.stderr);
    let ex = w.a(&["new", "--agent", "mock", "--project", "demo", "--branch", "main", "x"]).await;
    assert_eq!(ex.code, 1, "{}", ex.stderr);
    assert!(ex.stderr.contains("exists"), "{}", ex.stderr);
    // 非 git 会话的 diff 自动看「未提交」，不是仓库的报 not_git
    let ls = w.a(&["ls", "--json", "--all"]).await.json();
    let plain_id = ls["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["project"] == "plain")
        .map(|s| s["id"].as_str().unwrap().to_string())
        .unwrap();
    let nd = w.a(&["diff", &plain_id]).await;
    assert_eq!(nd.code, 2, "{}", nd.stderr);
    assert!(nd.stderr.contains("not_git"), "{}", nd.stderr);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_session_diffs_the_uncommitted_changes_and_has_no_branch_column() {
    let (w, daemon) = world().await;
    let repo = w.repo();
    let r = w.a(&["new", "--agent", "mock", "--project", "demo", "--here", "write locally"]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(repo.join("anycon-mock.txt").is_file());
    let d = w.a(&["diff", "last"]).await;
    assert_eq!(d.code, 0, "{}", d.stderr);
    assert!(d.stdout.contains("+mock write turn "), "{}", d.stdout);
    let stat = w.a(&["diff", "last", "--stat"]).await;
    assert!(stat.stdout.starts_with("main  1 uncommitted"), "{}", stat.stdout);
    let ls = w.a(&["ls"]).await;
    assert!(!ls.stdout.contains("BRANCH"), "{}", ls.stdout);
    // 本地会话没有 base，也没有可同步的东西
    let s = w.a(&["sync", "last"]).await;
    assert_eq!(s.code, 2, "{}", s.stderr);
    assert!(s.stderr.contains("not_worktree"), "{}", s.stderr);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_session_whose_worktree_never_got_created_does_not_claim_a_worktree_was_removed() {
    let (w, daemon) = world().await;
    let repo = w.repo();
    // `.anycon` 是个文件：worktree 建不出来，会话 `failed`
    std::fs::write(repo.join(".anycon"), "in the way").unwrap();
    let r = w.a(&["new", "--agent", "mock", "--project", "demo", "--worktree", "cannot be created"]).await;
    assert_ne!(r.code, 0, "{}", r.stdout);
    let ls = w.a(&["ls", "--json", "--all"]).await.json();
    let s = &ls["sessions"][0];
    assert_eq!(s["worktree"]["state"], "failed");
    let id = s["id"].as_str().unwrap().to_string();
    // 失败当时已经清理过，这次只删了记录：不能打印「worktree … removed」
    let del = w.a(&["delete", &id]).await;
    assert_eq!(del.code, 0, "{}", del.stderr);
    assert!(del.stdout.starts_with("deleted"), "{}", del.stdout);
    assert!(!del.stdout.contains("removed"), "{}", del.stdout);
    assert_eq!(std::fs::read_to_string(repo.join(".anycon")).unwrap(), "in the way");
    daemon.stop().await;
}
