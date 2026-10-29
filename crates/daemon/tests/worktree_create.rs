//! 每会话 worktree 的创建（ADR-34，docs/spec/daemon.md § 10.1「创建」）：真实 `git`，不联网；
//! daemon 的环境里 `GIT_CONFIG_GLOBAL` 指向临时文件，不读开发机的 `~/.gitconfig`。

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use anycon_protocol::*;
use common::wt::*;
use serde_json::json;

#[tokio::test]
async fn creating_a_worktree_session_makes_the_branch_the_directory_and_the_exclude_line() {
    let mut f = Fx::new().await;
    let mut watcher = f.td.client("watcher");
    let s = f.create_wt("Fix the login redirect bug").await;
    // 同步部分在返回之前做完：分支名、目录、状态 creating
    let wt = s.worktree.clone().expect("the summary carries the worktree");
    assert_eq!(wt.branch, "anycon/fix-the-login-redirect");
    assert_eq!(wt.path, ".anycon/worktrees/fix-the-login-redirect");
    assert_eq!(wt.base, "main");
    assert_eq!(wt.base_sha, f.td.git(&f.repo, &["rev-parse", "main"]));
    assert!(wt.id.starts_with("wt_") && wt.id.len() == 25, "{}", wt.id);
    assert_eq!(wt.state, WorktreeState::Creating);

    let s = f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let dir = f.dir_of(&s);
    // 分支、目录、登记
    assert!(f.branches().contains(&"anycon/fix-the-login-redirect".to_string()));
    assert!(dir.join("README.md").is_file(), "the base's files are checked out");
    assert_eq!(f.td.git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "anycon/fix-the-login-redirect");
    assert!(f.registered_worktrees().iter().any(|p| p.ends_with("/.anycon/worktrees/fix-the-login-redirect")));
    // `--no-track`：新分支没有上游
    assert!(f.td.git_raw(&dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]).status.code() != Some(0));
    // info/exclude 有一行 /.anycon/；仓库里的 .gitignore 没动
    let exclude = std::fs::read_to_string(f.repo.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.lines().filter(|l| l.trim() == "/.anycon/").count(), 1, "{exclude}");
    assert!(!f.repo.join(".gitignore").exists());
    assert_eq!(
        f.td.git(&f.repo, &["status", "--porcelain"]),
        "",
        "the worktree directory does not show up as untracked"
    );
    // agent 的 cwd 是 worktree 目录（不是项目目录）
    let spawns = f.td.fake.spawns.lock().unwrap().clone();
    assert_eq!(spawns.len(), 1);
    assert_eq!(
        anycon_daemon::sys::canonicalize(&spawns[0].cwd).unwrap(),
        anycon_daemon::sys::canonicalize(&dir).unwrap()
    );
    // git 事件在历史里：worktree_created 带分支、base、复制的文件数
    let evs = f.git_events(&s.id).await;
    assert_eq!(evs.len(), 1, "{evs:?}");
    match &evs[0] {
        Event::Git { action, branch, base, count, files, note } => {
            assert_eq!(*action, GitAction::WorktreeCreated);
            assert_eq!(branch, "anycon/fix-the-login-redirect");
            assert_eq!(base.as_deref(), Some("main"));
            assert_eq!(*count, Some(0));
            assert!(files.is_empty() && note.is_none());
        }
        other => panic!("{other:?}"),
    }
    // git 事件先于 agent 的第一个事件
    let kinds: Vec<String> = f
        .events(&s.id)
        .await
        .iter()
        .map(|e| serde_json::to_value(&e.event).unwrap()["type"].as_str().unwrap().to_string())
        .collect();
    let (g, t) =
        (kinds.iter().position(|k| k == "git").unwrap(), kinds.iter().position(|k| k == "text_delta").unwrap());
    assert!(g < t, "{kinds:?}");
    // session.changed 带完整的 SessionWorktree（SDK 把「没带」当「不变」）：创建时 creating，之后 active
    let notes = watcher.drain_for(Duration::from_millis(300)).await;
    let states: Vec<WorktreeState> = notes
        .iter()
        .filter(|n| n.method == methods::SESSION_CHANGED)
        .filter_map(|n| serde_json::from_value::<SessionChanged>(n.params.clone()).ok())
        .filter(|c| c.session_id == s.id)
        .filter_map(|c| c.worktree.map(|w| w.state))
        .collect();
    assert!(states.contains(&WorktreeState::Creating) && states.contains(&WorktreeState::Active), "{states:?}");
    assert_eq!(states.last(), Some(&WorktreeState::Active));
    // 审计 worktree.create：分支、base、路径
    let audit: NodeAuditResult = f.call(methods::NODE_AUDIT, json!({ "limit": 100 })).await.unwrap();
    let a = audit.entries.iter().find(|e| e.action == "worktree.create").expect("worktree.create is audited");
    assert_eq!(a.detail["branch"], "anycon/fix-the-login-redirect");
    assert_eq!(a.detail["base"], "main");
    assert_eq!(a.detail["path"], ".anycon/worktrees/fix-the-login-redirect");
    // 项目本身的工作区没被动过
    assert_eq!(f.td.git(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_second_message_reuses_the_worktree_and_the_session_stays_in_it() {
    let mut f = Fx::new().await;
    let s = f.ready("add a feature").await;
    f.send(&s.id, "and one more thing").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let evs = f.git_actions(&s.id).await;
    assert_eq!(evs, vec![GitAction::WorktreeCreated], "the worktree is created once");
    let spawns = f.td.fake.spawns.lock().unwrap().clone();
    assert!(spawns.iter().all(|sp| sp.cwd == spawns[0].cwd));
    f.td.shutdown().await;
}

#[tokio::test]
async fn names_come_from_english_words_or_a_random_pair_and_collisions_get_a_numeric_suffix() {
    let mut f = Fx::new().await;
    let a = f.create_wt("Refactor the parser").await;
    let b = f.create_wt("Refactor the parser").await;
    let c = f.create_wt("修复登录问题").await;
    let d = f.create(Some("proj"), None, Some(json!({ "mode": "worktree" }))).await.unwrap();
    let (wa, wb) = (a.worktree.unwrap(), b.worktree.unwrap());
    assert_eq!(wa.branch, "anycon/refactor-the-parser");
    assert_eq!(wb.branch, "anycon/refactor-the-parser-2");
    assert_eq!(wb.path, ".anycon/worktrees/refactor-the-parser-2");
    for s in [&c, &d] {
        let branch = s.worktree.as_ref().unwrap().branch.clone();
        let (prefix, slug) = branch.split_once('/').unwrap();
        assert_eq!(prefix, "anycon");
        assert!(slug.contains('-') && slug.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'), "{branch}");
    }
    // 同一时刻并发创建也拿到不同的名字
    let (mut c1, mut c2) = (f.td.client("c1"), f.td.client("c2"));
    let p = |t: &str| json!({ "agent": "fake", "project": "proj", "prompt": t, "message_id": uuid(), "workspace": { "mode": "worktree" } });
    let (r1, r2) = tokio::join!(
        c1.call::<SessionSummary>(&f.td, methods::SESSION_CREATE, p("same words here")),
        c2.call::<SessionSummary>(&f.td, methods::SESSION_CREATE, p("same words here")),
    );
    let (b1, b2) = (r1.unwrap().worktree.unwrap().branch, r2.unwrap().worktree.unwrap().branch);
    assert_ne!(b1, b2);
    f.td.shutdown().await;
}

#[tokio::test]
async fn create_validates_synchronously_before_returning() {
    let mut f = Fx::new().await;
    let wt = |extra: serde_json::Value| {
        let mut w = json!({ "mode": "worktree" });
        w.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        w
    };
    // scratch 会话带 worktree
    let e = f.create(None, Some("hi"), Some(json!({ "mode": "worktree" }))).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // 不是仓库的项目、没有提交的仓库、仓库里的子目录
    std::fs::create_dir_all(f.td.workspace.join("plain")).unwrap();
    let e = f.create(Some("plain"), Some("hi"), Some(json!({ "mode": "worktree" }))).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("not_worktree_ready")));
    let empty = f.td.workspace.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    f.td.git(&empty, &["init", "-q", "-b", "main"]);
    let e = f.create(Some("empty"), Some("hi"), Some(json!({ "mode": "worktree" }))).await.unwrap_err();
    assert_eq!(reason(&e), Some("not_worktree_ready"), "no commit yet");
    // 仓库里的子目录：只能靠「关联已有目录」当项目（根目录下的项目都是一级文件夹）
    let machine = anycon_daemon::sys::canonicalize(f.td.dir.path()).unwrap().join("machine");
    let outer = machine.join("outer");
    std::fs::create_dir_all(outer.join("sub")).unwrap();
    f.td.git(&outer, &["init", "-q", "-b", "main"]);
    f.td.commit_file(&outer, "a.txt", "a", "a");
    std::fs::create_dir_all(outer.join("sub")).unwrap();
    let roots = serde_json::to_string(&[machine.to_string_lossy()]).unwrap();
    anycon_daemon::config::set_value(&f.td.paths, "workspace.link_roots", &roots).unwrap();
    f.td.ctx.config.reload().unwrap();
    let sub = anycon_daemon::sys::canonicalize(outer.join("sub")).unwrap();
    let _: Project =
        f.call(methods::WORKSPACE_LINK, json!({ "path": sub.to_string_lossy(), "name": "subproj" })).await.unwrap();
    let e = f.create(Some("subproj"), Some("hi"), Some(json!({ "mode": "worktree" }))).await.unwrap_err();
    assert_eq!(reason(&e), Some("not_worktree_ready"), "a sub-directory of a repository is not the top level");
    // base
    for (base, why) in [("no-such-branch", "bad_base"), ("-x", "bad_base"), ("a\nb", "bad_base")] {
        let e = f.create(Some("proj"), Some("hi"), Some(wt(json!({ "base": base })))).await.unwrap_err();
        assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some(why)), "{base:?}");
    }
    // branch：已存在 / 保留名 / 不合法的名字
    f.td.git(&f.repo, &["branch", "taken"]);
    let e = f.create(Some("proj"), Some("hi"), Some(wt(json!({ "branch": "taken" })))).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Exists);
    let e = f.create(Some("proj"), Some("hi"), Some(wt(json!({ "branch": "main" })))).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Exists, "main exists");
    for bad in ["HEAD", "master", "a..b", "-dash", "has space", "end.lock", "@{-1}", "x/", ""] {
        let r = f.create(Some("proj"), Some("hi"), Some(wt(json!({ "branch": bad })))).await;
        match (bad, r) {
            ("", Ok(s)) => assert!(s.worktree.is_some(), "an empty branch means the default"),
            (_, Err(e)) => assert_eq!(code(&e), ErrorCode::Invalid, "{bad:?}: {e:?}"),
            (_, Ok(s)) => panic!("{bad:?} was accepted: {:?}", s.worktree),
        }
    }
    // 校验失败什么都没留下：没有会话行、没有 worktree 目录
    assert!(!f.repo.join(".anycon").join("worktrees").join("taken").exists());
    f.td.shutdown().await;
}

#[tokio::test]
async fn explicit_base_and_branch_and_the_default_base_follow_the_upstream() {
    let mut f = Fx::new().await;
    // 一个本地裸仓库当远端
    let remote = f.td.dir.path().join("remote.git");
    f.td.git(f.td.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
    let second = f.td.commit_file(&f.repo, "b.txt", "b", "second");
    f.td.git(&f.repo, &["push", "-q", "origin", "main"]);
    f.td.git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    f.td.commit_file(&f.repo, "f.txt", "f", "on feature");
    f.td.git(&f.repo, &["checkout", "-q", "main"]);
    // 缺省 base = 当前分支的上游（origin/main）
    let b: WorkspaceBranchesResult = f.call(methods::WORKSPACE_BRANCHES, json!({ "project": "proj" })).await.unwrap();
    assert_eq!(b.current.as_deref(), Some("main"));
    assert_eq!(b.default_base, "origin/main");
    let s = f.create_wt("use the upstream").await;
    assert_eq!(s.worktree.as_ref().unwrap().base, "origin/main");
    assert_eq!(s.worktree.as_ref().unwrap().base_sha, second);
    // 显式 base（本地分支）和显式分支名
    let s2 = f
        .create(
            Some("proj"),
            Some("x"),
            Some(json!({ "mode": "worktree", "base": "feature", "branch": "my/own-name" })),
        )
        .await
        .unwrap();
    let w2 = s2.worktree.unwrap();
    assert_eq!((w2.branch.as_str(), w2.base.as_str()), ("my/own-name", "feature"));
    assert_eq!(w2.path, ".anycon/worktrees/own-name", "the directory is the last segment of the branch");
    f.wait_wt_state(&s2.id, WorktreeState::Active).await;
    assert!(f.repo.join(".anycon/worktrees/own-name/f.txt").is_file(), "created from feature");
    // base 可以是一个提交
    let s3 = f.create(Some("proj"), Some("by sha"), Some(json!({ "mode": "worktree", "base": second }))).await.unwrap();
    assert_eq!(s3.worktree.unwrap().base_sha, second);
    // 当前分支没有上游时就是当前分支；detached 时是提交
    f.td.git(&f.repo, &["checkout", "-q", "feature"]);
    let b: WorkspaceBranchesResult = f.call(methods::WORKSPACE_BRANCHES, json!({ "project": "proj" })).await.unwrap();
    assert_eq!((b.current.as_deref(), b.default_base.as_str()), (Some("feature"), "feature"));
    f.td.git(&f.repo, &["checkout", "-q", "--detach", "main"]);
    let b: WorkspaceBranchesResult = f.call(methods::WORKSPACE_BRANCHES, json!({ "project": "proj" })).await.unwrap();
    assert_eq!(b.current, None);
    assert_eq!(b.default_base, second);
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_branch_list_leaves_out_anycons_own_branches_and_remote_heads() {
    let mut f = Fx::new().await;
    let remote = f.td.dir.path().join("remote.git");
    f.td.git(f.td.dir.path(), &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    f.td.git(&f.repo, &["push", "-q", "origin", "main"]);
    f.td.git(&f.repo, &["fetch", "-q", "origin"]);
    f.td.git(&f.repo, &["remote", "set-head", "origin", "main"]);
    f.td.git(&f.repo, &["branch", "anycon/old-one"]);
    f.td.git(&f.repo, &["branch", "topic"]);
    let b: WorkspaceBranchesResult = f.call(methods::WORKSPACE_BRANCHES, json!({ "project": "proj" })).await.unwrap();
    assert!(b.local.contains(&"main".to_string()) && b.local.contains(&"topic".to_string()), "{b:?}");
    assert!(!b.local.iter().any(|x| x.starts_with("anycon/")), "{b:?}");
    assert_eq!(b.remote, vec!["origin/main".to_string()], "origin/HEAD is not listed");
    assert!(!b.truncated);
    // 前缀改成别的，旧前缀的分支又出现了（热加载）
    anycon_daemon::config::set_value(&f.td.paths, "worktree.branch_prefix", "\"team\"").unwrap();
    f.td.ctx.config.reload().unwrap();
    let b: WorkspaceBranchesResult = f.call(methods::WORKSPACE_BRANCHES, json!({ "project": "proj" })).await.unwrap();
    assert!(b.local.contains(&"anycon/old-one".to_string()));
    // 非 git 项目
    std::fs::create_dir_all(f.td.workspace.join("plain")).unwrap();
    let e = f
        .call::<WorkspaceBranchesResult>(methods::WORKSPACE_BRANCHES, json!({ "project": "plain" }))
        .await
        .unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("not_git")));
    // 一页最多 200 个：多出来的标 truncated
    for i in 0..205 {
        f.td.git(&f.repo, &["branch", &format!("many-{i:03}")]);
    }
    let b: WorkspaceBranchesResult = f.call(methods::WORKSPACE_BRANCHES, json!({ "project": "proj" })).await.unwrap();
    assert_eq!(b.local.len(), 200);
    assert!(b.truncated);
    f.td.shutdown().await;
}

#[tokio::test]
async fn the_branch_prefix_is_configurable_and_hot() {
    let mut f = Fx::new().await;
    let a = f.create_wt("alpha task").await;
    assert_eq!(a.worktree.unwrap().branch, "anycon/alpha-task");
    anycon_daemon::config::set_value(&f.td.paths, "worktree.branch_prefix", "\"team-1\"").unwrap();
    f.td.ctx.config.reload().unwrap();
    let b = f.create_wt("beta task").await;
    assert_eq!(b.worktree.unwrap().branch, "team-1/beta-task");
    // 空串 = 不加前缀
    anycon_daemon::config::set_value(&f.td.paths, "worktree.branch_prefix", "\"\"").unwrap();
    f.td.ctx.config.reload().unwrap();
    let c = f.create_wt("gamma task").await;
    let w = c.worktree.unwrap();
    assert_eq!((w.branch.as_str(), w.path.as_str()), ("gamma-task", ".anycon/worktrees/gamma-task"));
    // 不合法的前缀写不进去
    for bad in
        ["main", "master", "HEAD", "has space", "a/b", "x".repeat(41).as_str(), ".dot", "-dash", "end.lock", "a..b"]
    {
        let r = anycon_daemon::config::set_value(&f.td.paths, "worktree.branch_prefix", &format!("\"{bad}\""));
        assert!(r.is_err(), "{bad:?} must be rejected");
    }
    f.td.shutdown().await;
}

#[tokio::test]
async fn node_info_announces_worktree_support_and_the_hot_config() {
    let mut f = Fx::new().await;
    let info: NodeInfo = f.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.worktree);
    let w = info.worktree.expect("settings are given when the node can do worktrees");
    assert_eq!((w.default_mode, w.branch_prefix.as_str()), (SessionWorkspaceMode::Worktree, "anycon"));
    anycon_daemon::config::set_values(
        &f.td.paths,
        &[("worktree.default", "\"local\""), ("worktree.branch_prefix", "\"zz\"")],
    )
    .unwrap();
    f.td.ctx.config.reload().unwrap();
    let info: NodeInfo = f.call(methods::NODE_INFO, json!({})).await.unwrap();
    let w = info.worktree.unwrap();
    assert_eq!((w.default_mode, w.branch_prefix.as_str()), (SessionWorkspaceMode::Local, "zz"));
    // 线路上不猜默认值：`worktree.default = local` 或 worktree 都不影响不带 workspace 的请求
    let s = f.create(Some("proj"), Some("hello"), None).await.unwrap();
    assert!(s.worktree.is_none());
    assert!(anycon_daemon::config::is_hot_key("worktree.default"));
    assert!(anycon_daemon::config::is_hot_key("worktree.include_max_bytes"));
    assert!(anycon_daemon::config::set_value(&f.td.paths, "worktree.default", "\"sideways\"").is_err());
    f.td.shutdown().await;
}

#[tokio::test]
async fn project_listing_says_which_projects_can_have_worktrees() {
    let mut f = Fx::new().await;
    std::fs::create_dir_all(f.td.workspace.join("plain")).unwrap();
    let empty = f.td.workspace.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    f.td.git(&empty, &["init", "-q", "-b", "main"]);
    // 5 秒缓存：第一次 list 可能先返回空的 git（50 ms 预算），等它跑完再看
    let mut ready = None;
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        let l: WorkspaceListResult = f.call(methods::WORKSPACE_LIST, json!({})).await.unwrap();
        let get = |n: &str| l.projects.iter().find(|p| p.name == n).unwrap().git.clone();
        if get("proj").is_some() && get("empty").is_some() {
            ready = Some((get("proj").unwrap(), get("empty").unwrap(), get("plain")));
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (proj, empty, plain) = ready.expect("git metadata shows up");
    assert!(proj.worktree_ready);
    assert!(!empty.worktree_ready, "no commit yet");
    assert!(plain.is_none());
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_failed_creation_cleans_up_and_ends_in_an_error_session() {
    let mut f = Fx::new().await;
    f.td.ctx.worktrees.hooks.fail_after_add.store(true, Ordering::SeqCst);
    let mut watcher = f.td.client("watcher");
    let s = f.create_wt("this will fail").await;
    let s = f.wait_wt_state(&s.id, WorktreeState::Failed).await;
    let s = f.wait_status(&s.id, SessionStatus::Error).await;
    // 磁盘上什么都没留下：目录、分支、登记
    assert!(!f.repo.join(".anycon/worktrees/this-will-fail").exists());
    assert!(!f.branches().iter().any(|b| b.starts_with("anycon/")), "{:?}", f.branches());
    assert_eq!(f.registered_worktrees().len(), 1, "only the main working tree");
    // 事件：worktree_failed（带原因）、error { worktree_failed, recoverable: false }；agent 没有被拉起
    let evs = f.events(&s.id).await;
    let failed = evs.iter().find_map(|e| match &e.event {
        Event::Git { action: GitAction::WorktreeFailed, note, .. } => Some(note.clone()),
        _ => None,
    });
    assert!(failed.flatten().is_some_and(|n| n.contains("injected")), "the reason is in the note");
    let error = evs.iter().find_map(|e| match &e.event {
        Event::Error { code, recoverable, .. } => Some((code.clone(), *recoverable)),
        _ => None,
    });
    assert_eq!(error, Some(("worktree_failed".to_string(), false)));
    assert_eq!(f.td.fake.spawn_count(), 0);
    assert!(evs.iter().any(|e| matches!(&e.event, Event::UserMessage { .. })), "the prompt stays in the history");
    // session.changed 带 failed
    let notes = watcher.drain_for(Duration::from_millis(200)).await;
    assert!(notes.iter().any(|n| {
        n.method == methods::SESSION_CHANGED
            && serde_json::from_value::<SessionChanged>(n.params.clone())
                .ok()
                .and_then(|c| c.worktree)
                .is_some_and(|w| w.state == WorktreeState::Failed)
    }));
    // 故障排除之后再发消息：重新尝试创建（沿用记下的分支名和目录名）
    f.td.ctx.worktrees.hooks.fail_after_add.store(false, Ordering::SeqCst);
    f.send(&s.id, "try again").await.unwrap();
    let s = f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert!(f.dir_of(&s).join("README.md").is_file());
    assert_eq!(f.td.fake.spawn_count(), 1);
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_symlinked_anycon_directory_is_refused_and_nothing_is_written_through_it() {
    let mut f = Fx::new().await;
    let outside = f.td.dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    anycon_daemon::testutil::symlink_dir(&outside, &f.repo.join(".anycon")).unwrap();
    let s = f.create_wt("through a link").await;
    let s = f.wait_wt_state(&s.id, WorktreeState::Failed).await;
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none(), "nothing was created behind the link");
    let evs = f.git_events(&s.id).await;
    assert!(
        matches!(&evs[0], Event::Git { action: GitAction::WorktreeFailed, note: Some(n), .. } if n.contains("symbolic link")),
        "{evs:?}"
    );
    // `.anycon` 是个文件也一样
    anycon_daemon::testutil::remove_link(&f.repo.join(".anycon")).unwrap();
    std::fs::write(f.repo.join(".anycon"), "file").unwrap();
    let s = f.create_wt("blocked by a file").await;
    f.wait_wt_state(&s.id, WorktreeState::Failed).await;
    assert_eq!(std::fs::read_to_string(f.repo.join(".anycon")).unwrap(), "file");
    f.td.shutdown().await;
}

#[tokio::test]
async fn agent_start_timeout_only_counts_after_the_worktree_exists() {
    let mut f = Fx::new().await;
    f.td.ctx.sessions.set_start_timeout(Duration::from_millis(600));
    // 建 worktree 比启动超时还久：不能因为 `starting` 太久被判超时
    f.td.ctx.worktrees.hooks.delay_after_add_ms.store(1500, Ordering::SeqCst);
    let s = f.create_wt("slow creation").await;
    assert_eq!(s.status, SessionStatus::Starting);
    let s = f.wait_status(&s.id, SessionStatus::Idle).await;
    assert_eq!(s.worktree.unwrap().state, WorktreeState::Active);
    let evs = f.events(&s.id).await;
    assert!(!evs.iter().any(|e| matches!(&e.event, Event::Error { code, .. } if code == "agent_start_timeout")));
    // 建好之后超时照常计：agent 迟迟不起来就是 agent_start_timeout
    f.td.ctx.worktrees.hooks.delay_after_add_ms.store(0, Ordering::SeqCst);
    f.td.fake.set_spawn_delay(Duration::from_secs(3));
    let s2 = f.create_wt("slow agent").await;
    f.wait_status(&s2.id, SessionStatus::Error).await;
    let evs = f.events(&s2.id).await;
    assert!(evs.iter().any(|e| matches!(&e.event, Event::Error { code, .. } if code == "agent_start_timeout")));
    f.td.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn worktreeinclude_copies_ignored_files_skips_links_and_respects_the_limits() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fx::new().await;
    // 被忽略的本地文件
    f.td.commit_file(&f.repo, ".gitignore", ".env\nnode_modules/\nlink.txt\nsecret.bin\n", "ignore");
    f.td.commit_file(&f.repo, ".worktreeinclude", ".env\nnode_modules/\nlink.txt\nsecret.bin\n.anycon/\n", "include");
    std::fs::write(f.repo.join(".env"), "TOKEN=dummy-value\n").unwrap();
    std::fs::set_permissions(f.repo.join(".env"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::create_dir_all(f.repo.join("node_modules/pkg")).unwrap();
    std::fs::write(f.repo.join("node_modules/pkg/index.js"), "module.exports = 1\n").unwrap();
    std::fs::write(f.repo.join("node_modules/pkg/run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(f.repo.join("node_modules/pkg/run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let outside = f.td.dir.path().join("outside.txt");
    std::fs::write(&outside, "do not copy").unwrap();
    std::os::unix::fs::symlink(&outside, f.repo.join("link.txt")).unwrap();
    std::fs::write(f.repo.join("not-listed.txt"), "untracked and not in the include file").unwrap();

    let s = f.ready("with includes").await;
    let dir = f.dir_of(&s);
    assert_eq!(std::fs::read_to_string(dir.join(".env")).unwrap(), "TOKEN=dummy-value\n");
    assert_eq!(
        std::fs::metadata(dir.join(".env")).unwrap().permissions().mode() & 0o777,
        0o600,
        "permissions are kept"
    );
    assert!(dir.join("node_modules/pkg/index.js").is_file());
    assert_eq!(std::fs::metadata(dir.join("node_modules/pkg/run.sh")).unwrap().permissions().mode() & 0o111, 0o111);
    assert!(std::fs::symlink_metadata(dir.join("link.txt")).is_err(), "symbolic links are not copied or followed");
    assert!(!dir.join("not-listed.txt").exists());
    assert!(!dir.join(".anycon").exists(), "the worktree directory itself is never copied into itself");
    // 复制的文件数在 git 事件的 count 里
    let evs = f.git_events(&s.id).await;
    match &evs[0] {
        Event::Git { count, note, .. } => {
            assert_eq!(*count, Some(3), "{note:?}");
            assert!(note.is_none());
        }
        other => panic!("{other:?}"),
    }

    // 文件数上限：到了就停下，note 说明没复制完
    anycon_daemon::config::set_value(&f.td.paths, "worktree.include_max_files", "1").unwrap();
    f.td.ctx.config.reload().unwrap();
    let s2 = f.ready("limited by count").await;
    let evs = f.git_events(&s2.id).await;
    match &evs[0] {
        Event::Git { count, note, .. } => {
            assert_eq!(*count, Some(1));
            assert!(note.as_deref().is_some_and(|n| n.contains("stopped copying")), "{note:?}");
        }
        other => panic!("{other:?}"),
    }
    // 字节数上限
    anycon_daemon::config::set_values(
        &f.td.paths,
        &[("worktree.include_max_files", "100"), ("worktree.include_max_bytes", "\"20B\"")],
    )
    .unwrap();
    f.td.ctx.config.reload().unwrap();
    let s3 = f.ready("limited by bytes").await;
    let evs = f.git_events(&s3.id).await;
    match &evs[0] {
        Event::Git { count, note, .. } => {
            assert!(count.unwrap() < 3);
            assert!(note.is_some());
        }
        other => panic!("{other:?}"),
    }
    f.td.shutdown().await;
}

#[tokio::test]
async fn creating_with_the_same_message_id_twice_makes_one_session_and_one_worktree() {
    let mut f = Fx::new().await;
    let mid = uuid();
    let p = json!({ "agent": "fake", "project": "proj", "prompt": "idempotent", "message_id": mid, "workspace": { "mode": "worktree" } });
    let a: SessionSummary = f.call(methods::SESSION_CREATE, p.clone()).await.unwrap();
    let b: SessionSummary = f.call(methods::SESSION_CREATE, p).await.unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(a.worktree.as_ref().unwrap().id, b.worktree.as_ref().unwrap().id);
    f.wait_wt_state(&a.id, WorktreeState::Active).await;
    assert_eq!(f.registered_worktrees().len(), 2);
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_session_without_a_prompt_gets_its_worktree_on_the_first_message() {
    let mut f = Fx::new().await;
    let s = f.create(Some("proj"), None, Some(json!({ "mode": "worktree" }))).await.unwrap();
    let wt = s.worktree.clone().unwrap();
    assert_eq!(wt.state, WorktreeState::Creating);
    // git 操作在拉起流程里：还没有 turn 的时候磁盘上什么都没有
    assert!(!f.repo.join(&wt.path).exists());
    // 此时看改动 / 状态：worktree 还在创建中
    let e = f.call::<SessionGitStatus>(methods::SESSION_GIT_STATUS, json!({ "session_id": s.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    f.send(&s.id, "go").await.unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    assert!(f.repo.join(&wt.path).join("README.md").is_file());
    f.td.shutdown().await;
}

#[tokio::test]
async fn without_git_on_the_path_the_node_says_so_and_refuses_worktrees() {
    use anycon_daemon::testutil::TestDaemon;
    let td = TestDaemon::start_without_git().await;
    let mut c = td.client("a");
    let info: NodeInfo = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(!info.capabilities.worktree);
    assert!(info.worktree.is_none(), "settings are only given when the node can do worktrees");
    std::fs::create_dir_all(td.workspace.join("proj")).unwrap();
    let e = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "proj", "prompt": "hi", "message_id": "m1", "workspace": { "mode": "worktree" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unsupported);
    // local 会话照常
    let s: SessionSummary = c
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "project": "proj", "prompt": "hi", "message_id": "m2" }),
        )
        .await
        .unwrap();
    assert!(s.worktree.is_none());
    td.shutdown().await;
}

#[tokio::test]
async fn worktree_sessions_survive_a_daemon_restart() {
    let mut f = Fx::new().await;
    let s = f.ready("persist me").await;
    let dir = f.dir_of(&s);
    f.td.restart().await;
    f.c = f.td.client("after-restart");
    let back = f.get(&s.id).await;
    let (before, after) = (s.worktree.clone().unwrap(), back.worktree.clone().unwrap());
    assert_eq!(after, before, "the worktree record is loaded from the database");
    f.send(&s.id, "after the restart").await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let spawns = f.td.fake.spawns.lock().unwrap().clone();
    assert_eq!(
        anycon_daemon::sys::canonicalize(&spawns.last().unwrap().cwd).unwrap(),
        anycon_daemon::sys::canonicalize(&dir).unwrap()
    );
    let list: WorkspaceWorktreesResult = f.call(methods::WORKSPACE_WORKTREES, json!({})).await.unwrap();
    assert_eq!(list.worktrees.len(), 1);
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_creation_cut_short_by_a_restart_is_redone_from_the_leftovers_on_the_next_send() {
    let mut f = Fx::new().await;
    // 旧实例：`git worktree add` 已经成功、后面还没记完成的时候，daemon 重启（崩溃的窗口）
    f.td.ctx.worktrees.hooks.delay_after_add_ms.store(2500, Ordering::SeqCst);
    let s = f.create_wt("cut short").await;
    let dir = f.repo.join(".anycon/worktrees/cut-short");
    let t = std::time::Instant::now();
    while !dir.exists() {
        assert!(t.elapsed() < T, "git worktree add never ran");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    f.td.restart().await;
    f.c = f.td.client("after-restart");
    let back = f.get(&s.id).await;
    assert_eq!(back.worktree.unwrap().state, WorktreeState::Creating, "the record never got past creating");
    assert_eq!(back.status, SessionStatus::Interrupted);
    // 新实例：下一次 send 把残局清掉（登记过的 worktree 用 git 删），沿用记下的分支名和目录名重新建
    f.send(&s.id, "try again after the restart").await.unwrap();
    let s = f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert!(dir.join("README.md").is_file());
    assert_eq!(s.worktree.unwrap().branch, "anycon/cut-short");
    assert_eq!(f.registered_worktrees().len(), 2, "exactly one linked worktree");
    assert_eq!(f.branches().iter().filter(|b| b.starts_with("anycon/")).count(), 1);
    f.td.shutdown().await;
}

#[tokio::test]
async fn cancelling_during_creation_and_sending_again_does_not_run_two_creations_at_once() {
    let mut f = Fx::new().await;
    f.td.ctx.worktrees.hooks.delay_after_add_ms.store(1200, Ordering::SeqCst);
    let s = f.create_wt("cancel then resend").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _: Empty = f.call(methods::SESSION_CANCEL, json!({ "session_id": s.id })).await.unwrap();
    f.wait_status(&s.id, SessionStatus::Idle).await;
    f.td.ctx.worktrees.hooks.delay_after_add_ms.store(0, Ordering::SeqCst);
    f.send(&s.id, "second attempt").await.unwrap();
    let s = f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    assert_eq!(f.registered_worktrees().len(), 2, "one linked worktree, created once");
    assert!(f.dir_of(&s).join("README.md").is_file());
    assert_eq!(f.td.fake.spawn_count(), 1, "the cancelled turn never started an agent");
    let evs = f.git_actions(&s.id).await;
    assert_eq!(evs, vec![GitAction::WorktreeCreated]);
    f.td.shutdown().await;
}
