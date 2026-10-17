//! worktree 相关集成测试共用的夹具：带 git 配置的 daemon、一个有提交的仓库项目、常用调用。

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::{json, Value};

/// git 比纯内存操作慢：等待上限放宽。
pub const T: Duration = Duration::from_secs(30);

pub struct Fx {
    pub td: TestDaemon,
    pub c: TestClient,
    pub repo: PathBuf,
}

pub fn code(e: &RpcError) -> ErrorCode {
    e.stable_code().expect("stable code")
}

pub fn reason(e: &RpcError) -> Option<&str> {
    e.data.as_ref().and_then(|d| d.reason.as_deref())
}

pub fn uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("wt-msg-{}", N.fetch_add(1, Ordering::SeqCst))
}

impl Fx {
    /// 带 git 夹具的 daemon + 项目 `proj`（`main` 分支上有一个提交）。
    pub async fn new() -> Fx {
        Self::with(&[]).await
    }

    pub async fn with(overrides: &[(&str, &str)]) -> Fx {
        let td = TestDaemon::start_git(overrides).await;
        let repo = td.new_repo("proj");
        let c = td.client("a");
        Fx { td, c, repo }
    }

    /// 已经启动好的 daemon（比如 `TestDaemon::start_mock_git`）和它的仓库项目。
    pub fn from_daemon(td: TestDaemon, repo: PathBuf) -> Fx {
        let c = td.client("a");
        Fx { td, c, repo }
    }

    pub async fn call<T: serde::de::DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<T, RpcError> {
        self.c.call(&self.td, method, params).await
    }

    /// `session.create`（agent = fake）。`workspace` 为 `None` 就不带这个字段（等于 local）。
    pub async fn create(
        &mut self,
        project: Option<&str>,
        prompt: Option<&str>,
        workspace: Option<Value>,
    ) -> Result<SessionSummary, RpcError> {
        let mut p = json!({ "agent": "fake" });
        if let Some(project) = project {
            p["project"] = json!(project);
        }
        if let Some(pr) = prompt {
            p["prompt"] = json!(pr);
            p["message_id"] = json!(uuid());
        }
        if let Some(w) = workspace {
            p["workspace"] = w;
        }
        self.call(methods::SESSION_CREATE, p).await
    }

    /// `proj` 里新 worktree 会话（缺省 base / 分支）。
    pub async fn create_wt(&mut self, prompt: &str) -> SessionSummary {
        self.create(Some("proj"), Some(prompt), Some(json!({ "mode": "worktree" }))).await.unwrap()
    }

    pub async fn get(&mut self, sid: &str) -> SessionSummary {
        self.call(methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap()
    }

    pub async fn wait<F: Fn(&SessionSummary) -> bool>(&mut self, sid: &str, what: &str, f: F) -> SessionSummary {
        let t = Instant::now();
        loop {
            let s = self.get(sid).await;
            if f(&s) {
                return s;
            }
            assert!(
                t.elapsed() < T,
                "timed out waiting for {what}; last: status {:?}, worktree {:?}",
                s.status,
                s.worktree
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn wait_status(&mut self, sid: &str, want: SessionStatus) -> SessionSummary {
        self.wait(sid, &format!("status {want:?}"), |s| s.status == want).await
    }

    pub async fn wait_wt_state(&mut self, sid: &str, want: WorktreeState) -> SessionSummary {
        self.wait(sid, &format!("worktree state {want:?}"), |s| s.worktree.as_ref().is_some_and(|w| w.state == want))
            .await
    }

    pub async fn send(&mut self, sid: &str, text: &str) -> Result<SessionSendResult, RpcError> {
        self.call(methods::SESSION_SEND, json!({ "session_id": sid, "message_id": uuid(), "content": text })).await
    }

    pub async fn events(&mut self, sid: &str) -> Vec<SessionEvent> {
        let r: SessionHistoryResult =
            self.call(methods::SESSION_HISTORY, json!({ "session_id": sid, "limit": 1000 })).await.unwrap();
        r.events
    }

    /// 历史里的 `git` 事件：(动作, 事件本身)。
    pub async fn git_events(&mut self, sid: &str) -> Vec<Event> {
        self.events(sid).await.into_iter().map(|e| e.event).filter(|e| matches!(e, Event::Git { .. })).collect()
    }

    pub async fn git_actions(&mut self, sid: &str) -> Vec<GitAction> {
        self.git_events(sid)
            .await
            .into_iter()
            .filter_map(|e| match e {
                Event::Git { action, .. } => Some(action),
                _ => None,
            })
            .collect()
    }

    /// worktree 目录（绝对路径）。
    pub fn dir_of(&self, s: &SessionSummary) -> PathBuf {
        self.repo.join(&s.worktree.as_ref().expect("a worktree session").path)
    }

    pub fn branches(&self) -> Vec<String> {
        self.td
            .git(&self.repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])
            .lines()
            .map(String::from)
            .collect()
    }

    pub fn registered_worktrees(&self) -> Vec<String> {
        self.td
            .git(&self.repo, &["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .map(String::from)
            .collect()
    }

    /// 建一个 worktree 会话并等它跑完第一个 turn（idle，worktree active）。
    pub async fn ready(&mut self, prompt: &str) -> SessionSummary {
        let s = self.create_wt(prompt).await;
        self.wait_wt_state(&s.id, WorktreeState::Active).await;
        self.wait_status(&s.id, SessionStatus::Idle).await
    }
}
