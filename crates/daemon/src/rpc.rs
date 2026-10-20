//! RPC 调度器：本机 Unix socket 和经 hub 的 Noise 对端共用同一个（ADR-27，docs/spec/daemon.md § 12）。
//!
//! 每个调用带 [`Caller`](crate::clients::Caller)（device_id / 名称 / 类型 / 是否本机），用于审计。方法名见 `anycon_protocol::methods`；
//! 另有仅本机可用的 `local.*` 方法（CLI 管理用），不属于客户端协议。

use std::sync::Arc;

use anycon_protocol::{
    methods, AgentDiagnoseParams, AgentListParams, BlobGetParams, BlobIdParams, BlobPutBeginParams, BlobPutChunkParams,
    Empty, ErrorCode, FsListParams, FsReadParams, JobCancelParams, JobStarted, NodeAuditParams, NodeAuditResult,
    NodeCapabilities, NodeDevicesResult, NodeEnrollApproveParams, NodeEnrollIdParams, NodeInfo, NodePairBeginParams,
    NodeRevokeDeviceParams, NodeSetAgentDefaultsParams, NodeSetEnrollEnabledParams, NodeSetEnrollPasswordParams,
    NodeStats, NodeWorktreeSettings, PermissionListParams, PermissionRespondParams, RenamedProject, RpcError, RpcId,
    RpcMessage, RpcRequest, SessionArchiveParams, SessionCreateParams, SessionDeleteParams, SessionDiffParams,
    SessionGitStatusParams, SessionHistoryParams, SessionIdParams, SessionListParams, SessionPromoteParams,
    SessionRenameParams, SessionSearchParams, SessionSearchResult, SessionSendParams, SessionSetSettingsParams,
    SessionSyncBaseParams, SessionWorkspaceMode, TrashEntry, WorkspaceBranchesParams, WorkspaceBrowseParams,
    WorkspaceCloneParams, WorkspaceCreateParams, WorkspaceLinkParams, WorkspaceNameParams,
    WorkspaceRemoveWorktreeParams, WorkspaceRenameParams, WorkspaceTrashIdParams, WorkspaceTrashListResult,
    WorkspaceWorktreesParams, PROTOCOL_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use crate::clients::ClientHandle;
use crate::daemon::Ctx;

type RpcResult<T> = Result<T, RpcError>;

/// 仅本机 socket 可用的管理方法（不属于客户端协议）。
pub mod local_methods {
    pub const STATUS: &str = "local.status";
    pub const RENAME_DEVICE: &str = "local.rename_device";
    pub const CONFIG_RELOAD: &str = "local.config_reload";
}

fn err(code: ErrorCode, msg: impl Into<String>) -> RpcError {
    RpcError::new(code, msg)
}

fn params<T: DeserializeOwned>(v: Value) -> RpcResult<T> {
    let v = if v.is_null() { json!({}) } else { v };
    serde_json::from_value(v).map_err(|e| err(ErrorCode::Invalid, format!("invalid params: {e}")))
}

fn ok<T: Serialize>(v: T) -> RpcResult<Value> {
    serde_json::to_value(v).map_err(|e| err(ErrorCode::Internal, e.to_string()))
}

pub struct Rpc {
    ctx: Arc<Ctx>,
}

impl Rpc {
    pub fn new(ctx: Arc<Ctx>) -> Arc<Rpc> {
        Arc::new(Rpc { ctx })
    }

    pub fn ctx(&self) -> &Arc<Ctx> {
        &self.ctx
    }

    /// 处理一个请求，产出对应的响应消息。
    /// 每个请求的处理耗时记一条 debug 日志（只有方法名和毫秒数，分析连接慢时用 `ANYCON_LOG=anycon_daemon=debug` 打开）。
    pub async fn handle_request(&self, client: &Arc<ClientHandle>, req: RpcRequest) -> RpcMessage {
        let id: RpcId = req.id.clone();
        let started = std::time::Instant::now();
        let res = self.dispatch(client, &req.method, req.params).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        tracing::debug!(method = %req.method, elapsed_ms, ok = res.is_ok(), "rpc handled");
        match res {
            Ok(v) => RpcMessage::result(id, v),
            Err(e) => {
                if e.stable_code() == Some(ErrorCode::Internal) {
                    tracing::warn!(method = %req.method, error = %e.message, "rpc internal error");
                }
                RpcMessage::error(id, e)
            }
        }
    }

    async fn dispatch(&self, client: &Arc<ClientHandle>, method: &str, p: Value) -> RpcResult<Value> {
        let c = &self.ctx;
        let caller = &client.caller;
        match method {
            // ---------------- node ----------------
            methods::NODE_INFO => ok(self.node_info()),
            methods::NODE_DEVICES => ok(NodeDevicesResult { devices: c.peers.list() }),
            methods::NODE_REVOKE_DEVICE => {
                let p: NodeRevokeDeviceParams = params(p)?;
                let id = c
                    .peers
                    .resolve(&p.device_id)
                    .ok_or_else(|| err(ErrorCode::NotFound, format!("device {} is not paired", p.device_id)))?;
                let hub_notified = c.peers.revoke(&id, Some(caller.device_id.as_str())).await?;
                c.audit.record(caller, "node.revoke_device", Some(&id), json!({ "hub_notified": hub_notified })).await;
                ok(Empty {})
            }
            methods::NODE_PAIR_BEGIN => {
                let p: NodePairBeginParams = params(p)?;
                ok(c.peers.pair_begin(caller, p.long).await?)
            }
            // ---------------- 账号内接入（ADR-36，daemon.md § 15.1）：本机 socket 上同样可用，set_by 记 "local" ----------------
            methods::NODE_ENROLL_LIST => ok(c.enroll.list()),
            methods::NODE_ENROLL_APPROVE => {
                let p: NodeEnrollApproveParams = params(p)?;
                ok(Box::pin(c.enroll.approve(caller, p)).await?)
            }
            methods::NODE_ENROLL_DENY => {
                let p: NodeEnrollIdParams = params(p)?;
                Box::pin(c.enroll.deny(caller, &p.enroll_id)).await?;
                ok(Empty {})
            }
            methods::NODE_ENROLL_CODE => ok(Box::pin(c.enroll.make_code(caller)).await?),
            methods::NODE_ENROLL_STATUS => ok(Box::pin(c.enroll.status()).await?),
            methods::NODE_SET_ENROLL_PASSWORD => {
                let p: NodeSetEnrollPasswordParams = params(p)?;
                Box::pin(c.enroll.set_password(caller, p.password)).await?;
                ok(Empty {})
            }
            methods::NODE_SET_ENROLL_ENABLED => {
                let p: NodeSetEnrollEnabledParams = params(p)?;
                Box::pin(c.enroll.set_enabled(caller, &c.paths, p)).await?;
                ok(Empty {})
            }
            methods::NODE_STATS => ok(self.node_stats().await),
            methods::NODE_DOCTOR => ok(anycon_protocol::NodeDoctorResult {
                checks: crate::doctor::run(&c.paths, &c.config.get(), Some(c)).await,
            }),
            methods::NODE_AUDIT => {
                let p: NodeAuditParams = params(p)?;
                let entries =
                    c.audit.query(p.since_ms, p.device_id.as_deref(), p.limit.unwrap_or(100).clamp(1, 1000)).await;
                ok(NodeAuditResult { entries })
            }
            methods::NODE_SET_AGENT_DEFAULTS => {
                let p: NodeSetAgentDefaultsParams = params(p)?;
                c.agents.set_defaults(&p).await?;
                let dangerous = match (&p.mode, c.agents.info(&p.agent)) {
                    (Some(m), Some(info)) => c.agents.is_dangerous_mode(&p.agent, &info, m),
                    _ => false,
                };
                c.audit
                    .record(
                        caller,
                        "node.set_agent_defaults",
                        Some(&p.agent),
                        json!({ "model": p.model, "thinking": p.thinking, "mode": p.mode, "dangerous": dangerous }),
                    )
                    .await;
                c.audit
                    .record(caller, "config.changed", Some(&format!("agents.{}.default_*", p.agent)), json!({}))
                    .await;
                ok(Empty {})
            }

            // ---------------- agent ----------------
            methods::AGENT_LIST => {
                let p: AgentListParams = params(p)?;
                ok(c.agents.handle_list(p).await)
            }
            methods::AGENT_DIAGNOSE => {
                let p: AgentDiagnoseParams = params(p)?;
                ok(c.agents.diagnose(&p.id).await?)
            }

            // ---------------- workspace ----------------
            methods::WORKSPACE_LIST => {
                let active = c.sessions.active_counts().await;
                ok(c.workspace.list(&active).await)
            }
            methods::WORKSPACE_CREATE => {
                let p: WorkspaceCreateParams = params(p)?;
                c.workspace.create(&p.name, p.git_init).await?;
                c.audit.record(caller, "workspace.create", Some(&p.name), json!({ "git_init": p.git_init })).await;
                ok(c.workspace.project(&p.name, 0).await?)
            }
            methods::WORKSPACE_CLONE => {
                let p: WorkspaceCloneParams = params(p)?;
                let job_id = c.workspace.clone_start(&p.url, p.name.as_deref()).await?;
                c.audit
                    .record(caller, "workspace.clone", p.name.as_deref(), json!({ "url": p.url, "job_id": job_id }))
                    .await;
                ok(JobStarted { job_id })
            }
            methods::WORKSPACE_RENAME => {
                let p: WorkspaceRenameParams = params(p)?;
                // 检查「项目忙」和给 worktree 会话标 git 忙是一个整体；改名、`git worktree repair`（worktree 的 `.git` 文件和
                // 主仓库里的记录都是绝对路径）都做完之后才让会话看到新名字（ADR-34）
                let marked = c.sessions.begin_project_rename(&p.from).await?;
                if let Err(e) = c.workspace.rename(&p.from, &p.to).await {
                    c.sessions.abort_project_rename(&marked).await;
                    return Err(e);
                }
                c.sessions.finish_project_rename(&marked, &p.from, &p.to).await;
                c.audit.record(caller, "workspace.rename", Some(&p.from), json!({ "to": p.to })).await;
                ok(RenamedProject { from: p.from, to: p.to })
            }
            methods::WORKSPACE_LINK => {
                let p: WorkspaceLinkParams = params(p)?;
                let project = c.workspace.link(&p.path, p.name.as_deref()).await?;
                // 同一个目录之前的会话（解除关联后换个名字关联回来）跟着改成新名字，旧会话可以继续
                let adopted = c.sessions.adopt_orphans(&project.path, &project.name).await;
                // 审计记 canonical 路径（不是客户端传来的原文）
                c.audit
                    .record(
                        caller,
                        "workspace.link",
                        Some(&project.name),
                        json!({ "path": project.path, "adopted_sessions": adopted }),
                    )
                    .await;
                ok(project)
            }
            methods::WORKSPACE_UNLINK => {
                let p: WorkspaceNameParams = params(p)?;
                // 「是不是关联项目」「有没有运行中的会话」「删除登记」在 Workspace 的同一把锁里
                let sessions = c.sessions.clone();
                let (name, path) =
                    c.workspace.unlink(&p.name, |name| async move { sessions.project_running(&name).await }).await?;
                // 锁释放前后才开始的 turn 取消掉，空闲的 agent 进程收掉
                c.sessions.release_project(&name).await;
                c.audit.record(caller, "workspace.unlink", Some(&name), json!({ "path": path })).await;
                ok(Empty {})
            }
            methods::WORKSPACE_BROWSE => {
                // 只列目录名，不读文件内容，不进审计
                let p: WorkspaceBrowseParams = params(p)?;
                ok(c.workspace.browse(p.path.as_deref(), p.show_hidden).await?)
            }
            methods::WORKSPACE_TRASH => {
                let p: WorkspaceNameParams = params(p)?;
                // 关联的目录永远不移动、不删除：先于 busy 检查，给出明确的 invalid
                c.workspace.ensure_not_linked_for_trash(&p.name)?;
                if c.sessions.project_busy(&p.name).await {
                    return Err(err(ErrorCode::Busy, "the project has running sessions"));
                }
                // worktree 在项目目录里，会跟着进回收站：还有没归档的 worktree 会话时不行
                if c.sessions.project_has_worktree_sessions(&p.name).await {
                    return Err(err(
                        ErrorCode::Busy,
                        "the project has sessions in their own worktrees; archive or delete them first",
                    ));
                }
                let entry: TrashEntry = c.workspace.trash(&p.name).await?;
                c.audit.record(caller, "workspace.trash", Some(&p.name), json!({ "trash_id": entry.id })).await;
                ok(entry)
            }
            methods::WORKSPACE_TRASH_LIST => ok(WorkspaceTrashListResult { entries: c.workspace.trash_list().await }),
            methods::WORKSPACE_RESTORE => {
                let p: WorkspaceTrashIdParams = params(p)?;
                let name = c.workspace.restore(&p.id).await?;
                c.worktrees.repair_project(&name).await;
                c.audit.record(caller, "workspace.restore", Some(&name), json!({ "trash_id": p.id })).await;
                ok(c.workspace.project(&name, 0).await?)
            }
            methods::WORKSPACE_PURGE => {
                let p: WorkspaceTrashIdParams = params(p)?;
                c.workspace.purge(&p.id).await?;
                c.audit.record(caller, "workspace.purge", Some(&p.id), json!({})).await;
                ok(Empty {})
            }
            methods::WORKSPACE_BRANCHES => {
                let p: WorkspaceBranchesParams = params(p)?;
                let dir = c.workspace.project_path(&p.project)?;
                ok(Box::pin(c.worktrees.branches(&dir)).await?)
            }
            methods::WORKSPACE_WORKTREES => {
                let p: WorkspaceWorktreesParams = params(p)?;
                ok(Box::pin(c.sessions.worktrees(p)).await?)
            }
            methods::WORKSPACE_REMOVE_WORKTREE => {
                let p: WorkspaceRemoveWorktreeParams = params(p)?;
                Box::pin(c.sessions.remove_worktree(caller, p)).await?;
                ok(Empty {})
            }
            methods::JOB_CANCEL => {
                let p: JobCancelParams = params(p)?;
                c.workspace.cancel_job(&p.job_id)?;
                ok(Empty {})
            }

            // ---------------- fs ----------------
            methods::FS_LIST => {
                let p: FsListParams = params(p)?;
                ok(c.workspace.fs_list(&p.path)?)
            }
            methods::FS_READ => {
                let p: FsReadParams = params(p)?;
                let r = c.workspace.fs_read(&p.path, p.max_bytes).await?;
                c.audit.record(caller, "fs.read", Some(&p.path), json!({ "bytes": r.bytes })).await;
                ok(r)
            }

            // ---------------- session ----------------
            methods::SESSION_LIST => {
                let p: SessionListParams = params(p)?;
                ok(c.sessions.list(p).await)
            }
            methods::SESSION_GET => {
                let p: SessionIdParams = params(p)?;
                ok(c.sessions.get_summary(&p.session_id).await?)
            }
            methods::SESSION_CREATE => {
                let p: SessionCreateParams = params(p)?;
                let summary = c.sessions.create(caller, p.clone()).await?;
                let dangerous = match (&summary.settings.mode, c.agents.info(&summary.agent)) {
                    (Some(m), Some(info)) => c.agents.is_dangerous_mode(&summary.agent, &info, m),
                    _ => false,
                };
                c.audit
                    .record(
                        caller,
                        "session.create",
                        Some(&summary.id),
                        json!({ "agent": summary.agent, "project": summary.project, "model": summary.settings.model, "thinking": summary.settings.thinking, "mode": summary.settings.mode, "dangerous": dangerous, "with_prompt": p.prompt.is_some(), "env_keys": p.settings.env.keys().collect::<Vec<_>>() }),
                    )
                    .await;
                ok(summary)
            }
            methods::SESSION_SEND => {
                let p: SessionSendParams = params(p)?;
                let len = p.content.chars().count();
                let sid = p.session_id.clone();
                let r = c.sessions.send(caller, p).await?;
                if !r.duplicate {
                    c.audit.record(caller, "session.send", Some(&sid), json!({ "chars": len })).await;
                }
                ok(r)
            }
            methods::SESSION_CANCEL => {
                let p: SessionIdParams = params(p)?;
                c.sessions.cancel(&p.session_id).await?;
                c.audit.record(caller, "session.cancel", Some(&p.session_id), json!({})).await;
                ok(Empty {})
            }
            methods::SESSION_SET_SETTINGS => {
                let p: SessionSetSettingsParams = params(p)?;
                let sid = p.session_id.clone();
                let requested = p.settings.clone();
                let cleared = p.clear.clone();
                let (r, dangerous) = c.sessions.set_settings(p).await?;
                c.audit.record(caller, "session.set_settings", Some(&sid), json!({ "model": requested.model, "thinking": requested.thinking, "mode": requested.mode, "dangerous": dangerous, "env_keys": requested.env.keys().collect::<Vec<_>>(), "cleared": cleared })).await;
                ok(r)
            }
            methods::SESSION_WATCH => {
                let p: SessionIdParams = params(p)?;
                c.sessions.get_summary(&p.session_id).await?;
                client.watch(&p.session_id).map_err(|_| {
                    err(
                        ErrorCode::Invalid,
                        format!("a client can watch at most {} sessions", crate::clients::MAX_WATCHES),
                    )
                })?;
                c.sessions.note_watched(&p.session_id);
                ok(Empty {})
            }
            methods::SESSION_UNWATCH => {
                let p: SessionIdParams = params(p)?;
                client.unwatch(&p.session_id);
                ok(Empty {})
            }
            methods::SESSION_HISTORY => {
                let p: SessionHistoryParams = params(p)?;
                ok(c.sessions.history(p).await?)
            }
            methods::SESSION_RENAME => {
                let p: SessionRenameParams = params(p)?;
                c.sessions.rename(&p.session_id, p.title).await?;
                ok(Empty {})
            }
            methods::SESSION_ARCHIVE => {
                // 只带 `session_id` 的旧请求照常合法（处置方式缺省 auto；没有 worktree 的会话不看它）
                let p: SessionArchiveParams = params(p)?;
                Box::pin(c.sessions.archive(caller, p)).await?;
                ok(Empty {})
            }
            methods::SESSION_UNARCHIVE => {
                let p: SessionIdParams = params(p)?;
                Box::pin(c.sessions.unarchive(&p.session_id)).await?;
                ok(Empty {})
            }
            methods::SESSION_DELETE => {
                let p: SessionDeleteParams = params(p)?;
                let (sid, disposal, delete_branch) = (p.session_id.clone(), p.worktree, p.delete_branch);
                Box::pin(c.sessions.delete_with(caller, p)).await?;
                c.audit
                    .record(
                        caller,
                        "session.delete",
                        Some(&sid),
                        json!({ "worktree": disposal, "delete_branch": delete_branch }),
                    )
                    .await;
                ok(Empty {})
            }
            methods::SESSION_GIT_STATUS => {
                let p: SessionGitStatusParams = params(p)?;
                ok(Box::pin(c.sessions.git_status(p)).await?)
            }
            methods::SESSION_DIFF => {
                let p: SessionDiffParams = params(p)?;
                ok(Box::pin(c.sessions.diff(p)).await?)
            }
            methods::SESSION_SYNC_BASE => {
                let p: SessionSyncBaseParams = params(p)?;
                ok(Box::pin(c.sessions.sync_base(caller, p)).await?)
            }
            methods::SESSION_RESTART => {
                let p: SessionIdParams = params(p)?;
                c.sessions.restart(&p.session_id).await?;
                ok(Empty {})
            }
            methods::SESSION_SEARCH => {
                let p: SessionSearchParams = params(p)?;
                ok(SessionSearchResult { hits: c.sessions.search(p).await? })
            }
            methods::SESSION_PROMOTE => {
                let p: SessionPromoteParams = params(p)?;
                c.sessions.promote(p.clone()).await?;
                c.audit
                    .record(caller, "workspace.create", Some(&p.name), json!({ "promoted_from": p.session_id }))
                    .await;
                ok(Empty {})
            }

            // ---------------- blob（附件，ADR-35；不进审计：量大、没有安全意义）----------------
            methods::BLOB_PUT_BEGIN => {
                let p: BlobPutBeginParams = params(p)?;
                ok(c.blobs.put_begin(&caller.device_id, p).await?)
            }
            methods::BLOB_PUT_CHUNK => {
                let p: BlobPutChunkParams = params(p)?;
                c.blobs.put_chunk(p).await?;
                ok(Empty {})
            }
            methods::BLOB_PUT_END => {
                let p: BlobIdParams = params(p)?;
                ok(c.blobs.put_end(p).await?)
            }
            methods::BLOB_GET => {
                let p: BlobGetParams = params(p)?;
                ok(c.blobs.get(p).await?)
            }
            methods::BLOB_DISCARD => {
                let p: BlobIdParams = params(p)?;
                c.blobs.discard(p).await?;
                ok(Empty {})
            }

            // ---------------- permission ----------------
            methods::PERMISSION_RESPOND => {
                let p: PermissionRespondParams = params(p)?;
                let request_id = p.request_id.clone();
                let message = p.message.clone();
                let (sid, option, switched) = c.sessions.respond_permission(caller, p).await?;
                c.audit
                    .record(
                        caller,
                        "permission.respond",
                        Some(&sid),
                        json!({ "request_id": request_id, "option_id": option, "message": message }),
                    )
                    .await;
                if let Some(sw) = switched {
                    c.audit
                        .record(
                            caller,
                            "session.set_settings",
                            Some(&sid),
                            json!({ "mode": sw.mode, "reason": "permission_approved", "dangerous": sw.dangerous }),
                        )
                        .await;
                }
                ok(Empty {})
            }
            methods::PERMISSION_LIST => {
                let p: PermissionListParams = params(p)?;
                ok(c.sessions.list_permissions(p).await)
            }

            // ---------------- local only ----------------
            m if m.starts_with("local.") => {
                if !caller.local {
                    return Err(err(ErrorCode::Forbidden, "this method is only available on the local socket"));
                }
                self.local_method(m, p).await
            }

            other => {
                let mut e = err(ErrorCode::Unsupported, format!("method not found: {other}"));
                e.code = -32601;
                Err(e)
            }
        }
    }

    async fn local_method(&self, method: &str, p: Value) -> RpcResult<Value> {
        let c = &self.ctx;
        match method {
            local_methods::STATUS => {
                let hub = c.hub_state.get();
                Ok(json!({
                    "pid": std::process::id(),
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol_version": PROTOCOL_VERSION,
                    "uptime_ms": c.started.elapsed().as_millis() as u64,
                    "home": c.paths.home,
                    "node_id": self.node_id(),
                    "name": c.config.get().node_name(),
                    "hub": {
                        "configured": hub.configured, "connected": hub.connected, "url": hub.url, "last_error": hub.last_error,
                        "auth_fail": hub.auth_fail, "clock_skew_ms": hub.clock_skew_ms, "device_id": hub.device_id,
                        "has_token": c.identity.device_token().is_some(),
                    },
                    "peers_online": c.peers.online_count(),
                    "env_source": c.env.source.as_str(),
                }))
            }
            local_methods::RENAME_DEVICE => {
                let device = p.get("device").and_then(Value::as_str).unwrap_or_default().to_string();
                let name = p.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let id = c
                    .peers
                    .resolve(&device)
                    .ok_or_else(|| err(ErrorCode::NotFound, format!("device {device} is not paired")))?;
                c.peers.rename(&id, &name).await?;
                Ok(json!({}))
            }
            local_methods::CONFIG_RELOAD => {
                let changed = c.config.reload().map_err(|e| err(ErrorCode::Invalid, e.to_string()))?;
                // 重读之后 daemon 对 `workspace.link_roots` 的判定：被忽略的项和目录不存在的项，各带原因。
                // 这是本机 CLI 专用的本地方法，不经过 hub，可以带完整的原因（包括符号链接的真实路径）
                Ok(json!({ "changed": changed, "link_roots_unusable": c.workspace.link_roots_report() }))
            }
            other => {
                let mut e = err(ErrorCode::Unsupported, format!("method not found: {other}"));
                e.code = -32601;
                Err(e)
            }
        }
    }

    pub fn node_id(&self) -> String {
        let c = &self.ctx;
        c.outbox
            .device_id()
            .filter(|_| c.identity.device_token().is_some())
            .or_else(|| c.identity.device_id())
            .unwrap_or_else(|| {
                format!("unpaired-{}", anycon_crypto::fingerprint(&c.identity.public()).replace(':', ""))
            })
    }

    fn node_info(&self) -> NodeInfo {
        let c = &self.ctx;
        NodeInfo {
            node_id: self.node_id(),
            name: c.config.get().node_name(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
            platform: crate::hub::platform(),
            workspace_root: Some(c.workspace.root().to_string_lossy().into_owned()),
            capabilities: NodeCapabilities {
                workspace: true,
                fs: true,
                search: true,
                blobs: true,
                audit: true,
                worktree: c.worktrees.available(),
            },
            started_at_ms: c.started_at_ms,
            // 每次现读配置（`[worktree]` 整段热加载）
            worktree: c.worktrees.available().then(|| {
                let cfg = c.config.get();
                NodeWorktreeSettings {
                    default_mode: if cfg.worktree.default == "local" {
                        SessionWorkspaceMode::Local
                    } else {
                        SessionWorkspaceMode::Worktree
                    },
                    branch_prefix: cfg.worktree.branch_prefix.clone(),
                }
            }),
        }
    }

    async fn node_stats(&self) -> NodeStats {
        let c = &self.ctx;
        let (sessions, procs) = c.sessions.counts().await;
        NodeStats {
            uptime_ms: c.started.elapsed().as_millis() as u64,
            hub_connected: c.hub_state.get().connected,
            peers_online: c.peers.online_count() as u32,
            sessions,
            agent_processes: procs,
            db_bytes: c.store.db_bytes(),
            disk_free_bytes: crate::doctor::disk_free(&c.paths.home),
        }
    }
}

/// 这些方法又慢又和别的请求没有先后依赖：不占串行队列，各自并发跑（否则 60 秒的探测会把同一客户端的
/// `permission.respond` 堵在后面）。
fn runs_detached(req: &RpcRequest) -> bool {
    match req.method.as_str() {
        methods::AGENT_DIAGNOSE | methods::NODE_DOCTOR => true,
        // git / gh 的调用可能很慢（`gh pr view` 20 秒、fetch 120 秒）：不能把同一客户端的 `permission.respond` 堵在后面。
        // 它们不依赖和别的请求的先后（`sync_base` 靠会话上的 `wt_busy` 标记和 `send` 互斥）
        methods::SESSION_GIT_STATUS
        | methods::SESSION_DIFF
        | methods::SESSION_SYNC_BASE
        | methods::WORKSPACE_BRANCHES
        | methods::WORKSPACE_WORKTREES => true,
        // 冷缓存时 `workspace.list` 要等 git 元数据（每个项目最多 3 秒）：不能把同一客户端后面的 `session.list` /
        // `permission.list` / `permission.respond` 挡 3 秒。它前面的请求在它之前已经处理完，后面的不依赖它
        methods::WORKSPACE_LIST => true,
        methods::AGENT_LIST => req.params.get("refresh").and_then(Value::as_bool).unwrap_or(false),
        _ => false,
    }
}

/// 同一个客户端的请求按到达顺序串行处理（protocol.md § 7.3）：每个客户端一个 worker，响应经 `out` 送回。
/// 上面几个慢方法例外，见 `runs_detached`。`rx` 的发送端全部 drop 后 worker 退出。
pub fn spawn_serial_worker(
    rpc: Arc<Rpc>,
    client: Arc<ClientHandle>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RpcRequest>,
    out: impl Fn(RpcMessage) + Send + Sync + 'static,
) -> tokio::task::JoinHandle<()> {
    let out = Arc::new(out);
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            if runs_detached(&req) {
                let (rpc, client, out) = (rpc.clone(), client.clone(), out.clone());
                tokio::spawn(async move { out(rpc.handle_request(&client, req).await) });
            } else {
                out(rpc.handle_request(&client, req).await);
            }
        }
    })
}

/// 把一条本机 / Noise 收到的原始字节解析为 RPC 请求（通知和响应忽略）。
pub fn parse_request(bytes: &[u8]) -> Result<Option<RpcRequest>, RpcError> {
    match RpcMessage::decode(bytes) {
        Ok(RpcMessage::Request(r)) => Ok(Some(r)),
        Ok(_) => Ok(None),
        Err(e) => Err(RpcError { code: -32700, message: format!("parse error: {e}"), data: None }),
    }
}

/// 构造解析失败时的响应（id 未知用 null 的替代：数字 0）。
pub fn parse_error_response(e: RpcError) -> RpcMessage {
    RpcMessage::error(RpcId::Num(0), e)
}
