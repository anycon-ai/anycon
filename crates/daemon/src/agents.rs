//! agent 注册表与探测调度（docs/spec/daemon.md § 5.3）。
//!
//! - 每个 agent 同一时间最多一个 probe 在跑，总超时 `agents.probe_timeout`（默认 60 秒）；
//! - 周期（`probe_interval`）只跑 cheap 探测；首次 / `refresh` / 命令或版本变了 / 该 agent 的配置变了才跑完整探测；
//! - 结果缓存在 `agent_cache` 表；`etag` 是整张列表（忽略 `probed_at_ms`）的哈希；变化时向所有在线客户端推 `agent.changed`。
//!
//! 没有 `[agents.<id>]` 配置段的 adapter 不参与（不出现在列表里）。daemon 对具体 adapter 一无所知。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anycon_agent_api::{reconcile_thinking, Adapter, AdapterConfig, ProbeContext};
use anycon_protocol::{
    methods, AgentDiagnoseResult, AgentInfo, AgentIssue, AgentListParams, AgentListResult, AgentStatus,
    ClearableSetting, DiagnosticField, ErrorCode, ModeRisk, RpcError, SessionSettings,
};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use crate::clients::Clients;
use crate::config::{ConfigStore, Paths};
use crate::env::DaemonEnv;
use crate::store::Store;
use crate::util::{now_ms, warn_err};

#[derive(Default)]
struct State {
    infos: BTreeMap<String, Arc<AgentInfo>>,
    diagnose: HashMap<String, String>,
}

pub struct Agents {
    adapters: Vec<Arc<dyn Adapter>>,
    config: Arc<ConfigStore>,
    env: Arc<DaemonEnv>,
    store: Store,
    clients: Arc<Clients>,
    paths: Paths,
    state: RwLock<State>,
    locks: HashMap<String, Arc<AsyncMutex<()>>>,
    /// 每个 agent 上一次生效的「影响探测」的配置指纹。
    fingerprints: Mutex<HashMap<String, String>>,
    wake: tokio::sync::Notify,
    /// 上一次向客户端广播（或确认）的 etag。配置变化导致 etag 变了才再推 `agent.changed`。
    announced_etag: Mutex<String>,
}

fn strip_probed(i: &AgentInfo) -> AgentInfo {
    let mut c = i.clone();
    c.probed_at_ms = 0;
    c
}

/// 选项值要和 `AgentInfo.options[]` 声明的类型一致：bool 是布尔、select 是某个 choice 的 id、text 是（不太长的）字符串。
/// 非法值如果放进会话，会让之后每次 spawn 都失败，所以在写入前就挡住。
fn validate_option_values(
    info: &AgentInfo,
    options: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), RpcError> {
    use anycon_protocol::ConfigOptionKind;
    for (k, v) in options {
        let Some(opt) = info.options.iter().find(|o| &o.id == k) else {
            return Err(RpcError::invalid_settings(format!("options.{k}"), "unknown option"));
        };
        let ok = match opt.kind {
            ConfigOptionKind::Bool => v.is_boolean(),
            ConfigOptionKind::Select => {
                v.as_str().is_some_and(|x| opt.choices.is_empty() || opt.choices.iter().any(|c| c.id == x))
            }
            ConfigOptionKind::Text => v.as_str().is_some_and(|x| x.len() <= 4096),
        };
        if !ok {
            let want = match opt.kind {
                ConfigOptionKind::Bool => "a boolean".to_string(),
                ConfigOptionKind::Select => {
                    format!("one of {}", opt.choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", "))
                }
                ConfigOptionKind::Text => "a string of at most 4096 bytes".to_string(),
            };
            return Err(RpcError::invalid_settings(format!("options.{k}"), format!("expected {want}")));
        }
    }
    Ok(())
}

fn compute_etag(infos: &[AgentInfo]) -> String {
    let list: Vec<AgentInfo> = infos.iter().map(strip_probed).collect();
    let bytes = serde_json::to_vec(&list).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    hex::encode(&digest[..8])
}

/// 客户端看到的 `AgentInfo`：在探测结果上叠加配置。
/// - `agents.<id>.default_model / default_thinking / default_mode / default_options` 反映到各项的 `default` 标记，
///   这样所有客户端都能读到节点默认（没有单独的读接口）；
/// - `agents.<id>.dangerous_modes` 把相应模式标成 dangerous；
/// - `permissions.allow_dangerous_modes = false` 时把 dangerous 模式从 `modes` 里去掉（客户端据此不提供危险模式）。
///   校验仍以完整的探测结果为准，直接指定会得到 `forbidden`。
pub fn effective_info(raw: &AgentInfo, cfg: &crate::config::Config) -> AgentInfo {
    let mut i = raw.clone();
    let Some(ac) = cfg.adapter_config(&i.id) else { return i };
    for m in &mut i.modes {
        if ac.dangerous_modes.contains(&m.id) {
            m.risk = ModeRisk::Dangerous;
        }
    }
    if !cfg.permissions.allow_dangerous_modes {
        i.modes.retain(|m| m.risk != ModeRisk::Dangerous);
    }
    if let Some(dm) = ac.default_model.as_ref().filter(|dm| i.models.iter().any(|m| &m.id == *dm)) {
        for m in &mut i.models {
            m.default = &m.id == dm;
        }
    }
    if let Some(dt) = &ac.default_thinking {
        for m in &mut i.models {
            if let Some(t) = &mut m.thinking {
                if t.options.iter().any(|o| &o.id == dt) {
                    t.default = Some(dt.clone());
                }
            }
        }
    }
    if let Some(dm) = ac.default_mode.as_ref().filter(|dm| i.modes.iter().any(|m| &m.id == *dm)) {
        for m in &mut i.modes {
            m.default = &m.id == dm;
        }
    }
    for o in &mut i.options {
        if let Some(v) = ac.default_options.get(&o.id) {
            o.default = v.clone();
        }
    }
    i
}

/// 配置里影响探测结果的部分（排除 `default_*`）。键排序后再比较，不受 HashMap 顺序影响。
fn probe_fingerprint(cfg: &crate::config::Config, id: &str) -> String {
    let Some(ac) = cfg.adapter_config(id) else { return "absent".into() };
    let mut v = serde_json::to_value(ac).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.retain(|k, _| !k.starts_with("default_"));
    }
    let g = serde_json::to_value(&cfg.agents.env).unwrap_or_default();
    format!("{}|{}", crate::util::canonical_json(&v), crate::util::canonical_json(&g))
}

impl Agents {
    pub async fn new(
        adapters: Vec<Arc<dyn Adapter>>,
        config: Arc<ConfigStore>,
        env: Arc<DaemonEnv>,
        store: Store,
        clients: Arc<Clients>,
        paths: Paths,
    ) -> Arc<Self> {
        let locks = adapters.iter().map(|a| (a.id().to_string(), Arc::new(AsyncMutex::new(())))).collect();
        let this = Arc::new(Agents {
            adapters,
            config,
            env,
            store,
            clients,
            paths,
            state: RwLock::new(State::default()),
            locks,
            fingerprints: Mutex::new(HashMap::new()),
            wake: tokio::sync::Notify::new(),
            announced_etag: Mutex::new(String::new()),
        });
        this.seed_from_cache().await;
        this
    }

    /// 先用缓存和占位填满列表：客户端一连上就有东西可画，完整探测在后台跑。
    async fn seed_from_cache(&self) {
        let cached: HashMap<String, AgentInfo> = self
            .store
            .load_agent_cache()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(i, _)| (i.id.clone(), i))
            .collect();
        let cfg = self.config.get();
        let mut st = self.state.write().unwrap();
        for a in &self.adapters {
            let Some(ac) = cfg.adapter_config(a.id()) else { continue };
            let info = if !ac.enabled {
                self.disabled_info(a.as_ref())
            } else if let Some(c) = cached.get(a.id()) {
                let mut c = c.clone();
                c.enabled = true;
                c
            } else {
                self.placeholder(a.as_ref(), AgentStatus::Loading)
            };
            st.infos.insert(a.id().to_string(), Arc::new(info));
            self.fingerprints.lock().unwrap().insert(a.id().to_string(), probe_fingerprint(&cfg, a.id()));
        }
    }

    fn placeholder(&self, a: &dyn Adapter, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            id: a.id().to_string(),
            label: a.label().to_string(),
            driver: a.driver().to_string(),
            enabled: true,
            status,
            version: None,
            path: None,
            issue: None,
            models: vec![],
            allow_custom_model: false,
            modes: vec![],
            options: vec![],
            capabilities: Default::default(),
            probed_at_ms: 0,
        }
    }

    fn disabled_info(&self, a: &dyn Adapter) -> AgentInfo {
        let mut i = self.placeholder(a, AgentStatus::Disabled);
        i.enabled = false;
        i.probed_at_ms = now_ms();
        i
    }

    pub fn adapter(&self, id: &str) -> Option<Arc<dyn Adapter>> {
        self.adapters.iter().find(|a| a.id() == id).cloned()
    }

    pub fn info(&self, id: &str) -> Option<Arc<AgentInfo>> {
        self.state.read().unwrap().infos.get(id).cloned()
    }

    /// 客户端看到的列表：探测结果 + 配置的叠加（见 [`effective_info`]）。
    pub fn list(&self) -> AgentListResult {
        let cfg = self.config.get();
        let agents: Vec<AgentInfo> =
            self.state.read().unwrap().infos.values().map(|i| effective_info(i, &cfg)).collect();
        let etag = compute_etag(&agents);
        AgentListResult { agents, etag, not_modified: false }
    }

    pub fn etag(&self) -> String {
        self.list().etag
    }

    /// `agent.list`。
    pub async fn handle_list(self: &Arc<Self>, p: AgentListParams) -> AgentListResult {
        if p.refresh {
            self.refresh_all().await;
        }
        let mut r = self.list();
        if p.if_none_match.as_deref() == Some(r.etag.as_str()) && !p.refresh {
            r.agents = vec![];
            r.not_modified = true;
        }
        r
    }

    /// 对所有参与的 agent 做完整探测并等待完成。
    pub async fn refresh_all(self: &Arc<Self>) {
        let ids = self.participating_ids();
        let mut set = tokio::task::JoinSet::new();
        for id in ids {
            let this = self.clone();
            set.spawn(async move { this.probe(&id, false).await });
        }
        while set.join_next().await.is_some() {}
    }

    fn participating_ids(&self) -> Vec<String> {
        let cfg = self.config.get();
        self.adapters.iter().filter(|a| cfg.adapter_config(a.id()).is_some()).map(|a| a.id().to_string()).collect()
    }

    /// 探测一个 agent。`cheap = true` 只跑便宜的步骤（除非需要升级为完整探测）。
    pub async fn probe(self: &Arc<Self>, id: &str, cheap: bool) -> Option<Arc<AgentInfo>> {
        let adapter = self.adapter(id)?;
        let lock = self.locks.get(id)?.clone();
        // 周期性 cheap 探测碰到正在跑的探测就跳过；强制探测排队等
        let _guard = if cheap {
            match lock.try_lock() {
                Ok(g) => g,
                Err(_) => return self.info(id),
            }
        } else {
            lock.lock().await
        };
        let cfg = self.config.get();
        let Some(ac) = cfg.adapter_config(id).cloned() else {
            self.remove(id);
            return None;
        };
        self.fingerprints.lock().unwrap().insert(id.to_string(), probe_fingerprint(&cfg, id));
        if !ac.enabled {
            let info = self.disabled_info(adapter.as_ref());
            self.commit(info, None).await;
            return self.info(id);
        }
        let prev = self.info(id);
        let need_full = !cheap
            || prev.as_ref().is_none_or(|p| {
                matches!(p.status, AgentStatus::Loading | AgentStatus::Disabled) || p.probed_at_ms == 0
            });
        let mut info = self.run_probe(adapter.as_ref(), &cfg, &ac, prev.as_deref(), !need_full).await;
        // cheap 探测发现路径或版本变了：马上补一次完整探测
        if !need_full {
            if let Some(p) = &prev {
                if (p.path != info.path || p.version != info.version) && info.status == AgentStatus::Ready {
                    info = self.run_probe(adapter.as_ref(), &cfg, &ac, prev.as_deref(), false).await;
                }
            }
        }
        self.commit(info, None).await;
        self.info(id)
    }

    async fn run_probe(
        &self,
        adapter: &dyn Adapter,
        cfg: &crate::config::Config,
        ac: &AdapterConfig,
        prev: Option<&AgentInfo>,
        cheap: bool,
    ) -> AgentInfo {
        let scratch = self.paths.probe_scratch();
        let _ = std::fs::create_dir_all(&scratch);
        let deadline = cfg.probe_timeout();
        let ctx = ProbeContext {
            env: self.env.for_agent(cfg, adapter.id()),
            config: ac.clone(),
            deadline,
            cheap,
            previous: prev.cloned(),
            scratch_dir: scratch,
        };
        let mut info = match tokio::time::timeout(deadline + Duration::from_secs(2), adapter.probe(&ctx)).await {
            Ok(i) => i,
            Err(_) => {
                let mut i = prev.cloned().unwrap_or_else(|| self.placeholder(adapter, AgentStatus::Error));
                i.status = AgentStatus::Error;
                i.issue = Some(AgentIssue {
                    code: "probe_timeout".into(),
                    message: format!("probe did not finish within {}s", deadline.as_secs()),
                    hint: Some("run `anycon doctor` to see which step is slow".into()),
                });
                i
            }
        };
        // 身份字段以 adapter 为准，防止 adapter 填错
        info.id = adapter.id().to_string();
        if info.label.is_empty() {
            info.label = adapter.label().to_string();
        }
        if info.driver.is_empty() {
            info.driver = adapter.driver().to_string();
        }
        info.enabled = true;
        if info.probed_at_ms == 0 {
            info.probed_at_ms = now_ms();
        }
        info
    }

    fn remove(&self, id: &str) {
        self.state.write().unwrap().infos.remove(id);
    }

    /// 写入新结果；内容（忽略 probed_at）有变化才更新 etag、写缓存、推 `agent.changed`。
    async fn commit(&self, info: AgentInfo, diagnose: Option<String>) {
        let changed = {
            let mut st = self.state.write().unwrap();
            let changed = st.infos.get(&info.id).is_none_or(|old| strip_probed(old) != strip_probed(&info));
            st.infos.insert(info.id.clone(), Arc::new(info.clone()));
            if let Some(d) = &diagnose {
                st.diagnose.insert(info.id.clone(), d.clone());
            }
            changed
        };
        warn_err("save the agent cache", self.store.save_agent_cache(&info, diagnose.as_deref()).await);
        if changed {
            tracing::info!(agent = %info.id, status = ?info.status, version = ?info.version, "agent probe result changed");
            self.announce();
        }
    }

    /// 向所有在线客户端推 `agent.changed`（完整列表）。
    fn announce(&self) {
        let list = self.list();
        *self.announced_etag.lock().unwrap() = list.etag.clone();
        self.clients.broadcast(methods::AGENT_CHANGED, list);
    }

    /// 配置变化（`default_*`、`allow_dangerous_modes`、`dangerous_modes`）改变了客户端看到的列表时补推一次。
    fn announce_if_view_changed(&self) {
        let etag = self.etag();
        let changed = {
            let mut last = self.announced_etag.lock().unwrap();
            let changed = !last.is_empty() && *last != etag;
            if changed || last.is_empty() {
                *last = etag;
            }
            changed
        };
        if changed {
            self.announce();
        }
    }

    /// 会话以 agent_not_found / auth_failed 失败后调用：马上重探。
    pub fn reprobe_soon(self: &Arc<Self>, id: &str) {
        let this = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            this.probe(&id, false).await;
        });
    }

    /// 睡眠唤醒后：重跑探测（便宜的；之前不 ready 的做完整探测）。
    pub fn reprobe_all_soon(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            for id in this.participating_ids() {
                let ready = this.info(&id).is_some_and(|i| i.status == AgentStatus::Ready);
                this.probe(&id, ready).await;
            }
        });
    }

    /// 后台任务：首次完整探测 → 周期 cheap 探测 → 配置变化时重探。返回 JoinHandle，daemon 退出时 abort。
    pub fn spawn_scheduler(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            this.refresh_all().await;
            let mut cfg_rx = this.config.subscribe();
            loop {
                let interval = this.config.get().probe_interval();
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        for id in this.participating_ids() {
                            let this2 = this.clone();
                            tokio::spawn(async move { this2.probe(&id, true).await; });
                        }
                    }
                    r = cfg_rx.changed() => {
                        if r.is_err() { return; }
                        this.on_config_changed().await;
                    }
                    _ = this.wake.notified() => {}
                }
            }
        })
    }

    async fn on_config_changed(self: &Arc<Self>) {
        self.announce_if_view_changed();
        let cfg = self.config.get();
        let mut to_probe = Vec::new();
        for a in &self.adapters {
            let id = a.id();
            let fp = probe_fingerprint(&cfg, id);
            let changed = self.fingerprints.lock().unwrap().get(id).is_none_or(|old| old != &fp);
            if changed {
                if cfg.adapter_config(id).is_none() {
                    self.remove(id);
                    self.fingerprints.lock().unwrap().insert(id.to_string(), fp);
                } else {
                    to_probe.push(id.to_string());
                }
            }
        }
        for id in to_probe {
            let this = self.clone();
            tokio::spawn(async move {
                this.probe(&id, false).await;
            });
        }
        // 仅 default_* 变化：不重探，但列表里的 `default` 标记不依赖它们，所以无事可做
    }

    // ---------------- 会话设置：默认值与校验 ----------------

    /// `session.create` 不传的字段：先取 `config.agents.<id>.default_*`，再取 AgentInfo 里标 default 的项。
    pub fn apply_defaults(&self, id: &str, info: &AgentInfo, mut s: SessionSettings) -> SessionSettings {
        let cfg = self.config.get();
        let ac = cfg.adapter_config(id).cloned().unwrap_or_default();
        if s.model.is_none() {
            let cfg_model =
                ac.default_model.clone().filter(|m| info.allow_custom_model || info.models.iter().any(|x| &x.id == m));
            s.model = cfg_model.or_else(|| info.models.iter().find(|m| m.default).map(|m| m.id.clone()));
        }
        if s.thinking.is_none() {
            if let Some(model) = s.model.as_ref().and_then(|m| info.models.iter().find(|x| &x.id == m)) {
                if let Some(t) = &model.thinking {
                    let cfg_th = ac.default_thinking.clone().filter(|th| t.options.iter().any(|o| &o.id == th));
                    s.thinking = cfg_th.or_else(|| t.default.clone());
                }
            }
        }
        if s.mode.is_none() {
            let cfg_mode = ac.default_mode.clone().filter(|m| info.modes.iter().any(|x| &x.id == m));
            s.mode = cfg_mode.or_else(|| info.modes.iter().find(|m| m.default).map(|m| m.id.clone()));
        }
        for (k, v) in &ac.default_options {
            if info.options.iter().any(|o| &o.id == k) && !s.options.contains_key(k) {
                s.options.insert(k.clone(), v.clone());
            }
        }
        s
    }

    /// 校验设置：会话 env 策略（ADR-31）+ adapter 自己的规则 + 选项值 + 数值范围 + `permissions.allow_dangerous_modes` 门禁。
    ///
    /// 每次 spawn 之前（含重启 / resume）、`session.create`、`session.set_settings`、`node.set_agent_defaults`
    /// 都走这里：配置可以热改，所以不能只在创建时检查一次。
    pub fn validate_settings(&self, id: &str, info: &AgentInfo, s: &SessionSettings) -> Result<(), RpcError> {
        let adapter = self
            .adapter(id)
            .ok_or_else(|| RpcError::new(ErrorCode::AgentUnavailable, format!("unknown agent {id}")))?;
        let cfg = self.config.get();
        if let Err((field, reason)) = crate::env::validate_session_env(&cfg, id, &s.env) {
            return Err(RpcError::invalid_settings(field, reason));
        }
        match adapter.validate_settings(info, s) {
            Ok(()) => {}
            Err(anycon_agent_api::AdapterError::InvalidSettings { field, reason }) => {
                return Err(RpcError::invalid_settings(field, reason))
            }
            Err(e) => return Err(RpcError::new(ErrorCode::Invalid, e.to_string())),
        }
        validate_option_values(info, &s.options)?;
        if let Some(b) = s.max_budget_usd {
            if !b.is_finite() || b < 0.0 {
                return Err(RpcError::invalid_settings("max_budget_usd", "must be a finite number >= 0"));
            }
        }
        if s.system_prompt_append.as_ref().is_some_and(|t| t.len() > 16 * 1024) {
            return Err(RpcError::invalid_settings("system_prompt_append", "at most 16 KiB"));
        }
        if let Some(mode) = &s.mode {
            if self.is_dangerous_mode(id, info, mode) && !cfg.permissions.allow_dangerous_modes {
                return Err(RpcError::new(
                    ErrorCode::Forbidden,
                    format!("mode {mode} is dangerous and permissions.allow_dangerous_modes is false"),
                ));
            }
        }
        Ok(())
    }

    /// 某个 mode 是否是危险模式（审计用）。
    pub fn is_dangerous_mode(&self, id: &str, info: &AgentInfo, mode: &str) -> bool {
        let cfg = self.config.get();
        cfg.adapter_config(id).is_some_and(|c| c.dangerous_modes.iter().any(|m| m == mode))
            || info.modes.iter().any(|m| m.id == mode && m.risk == ModeRisk::Dangerous)
    }

    /// 合并增量设置：`new` 里是 `Some` 的字段覆盖，`None`（缺省 / `null`）保持；options 逐键合并（值为 `null` 删键），env 逐键合并。
    /// 合并之后再处理 `clear`（点名的字段恢复为未设置：`env` 清空整个表，`model` / `thinking` / `mode` 回到默认，见
    /// [`Agents::refill_cleared`]）。只改 model 而没给 thinking 时，thinking 回落到新模型的默认。
    /// 返回 (合并结果, 是否做了 thinking 回落)。
    pub fn merge_settings(
        info: &AgentInfo,
        old: &SessionSettings,
        new: &SessionSettings,
        clear: &[ClearableSetting],
    ) -> (SessionSettings, bool) {
        let mut out = old.clone();
        if new.model.is_some() {
            out.model = new.model.clone();
        }
        if new.thinking.is_some() {
            out.thinking = new.thinking.clone();
        }
        if new.mode.is_some() {
            out.mode = new.mode.clone();
        }
        for (k, v) in &new.options {
            if v.is_null() {
                out.options.remove(k);
            } else {
                out.options.insert(k.clone(), v.clone());
            }
        }
        for (k, v) in &new.env {
            out.env.insert(k.clone(), v.clone());
        }
        if new.system_prompt_append.is_some() {
            out.system_prompt_append = new.system_prompt_append.clone();
        }
        if new.max_turns.is_some() {
            out.max_turns = new.max_turns;
        }
        if new.max_budget_usd.is_some() {
            out.max_budget_usd = new.max_budget_usd;
        }
        let mut reconciled = false;
        if new.model.is_some() && new.model != old.model && new.thinking.is_none() {
            reconciled = reconcile_thinking(info, &mut out);
        }
        // `clear` 在合并之后生效；两边都出现时以 `clear` 为准
        for c in clear {
            match c {
                ClearableSetting::Model => out.model = None,
                ClearableSetting::Thinking => out.thinking = None,
                ClearableSetting::Mode => out.mode = None,
                ClearableSetting::Env => out.env.clear(),
                ClearableSetting::SystemPromptAppend => out.system_prompt_append = None,
                ClearableSetting::MaxTurns => out.max_turns = None,
                ClearableSetting::MaxBudgetUsd => out.max_budget_usd = None,
            }
        }
        (out, reconciled)
    }

    /// `model` / `thinking` / `mode` 被 `clear` 之后回到默认（先 `agents.<id>.default_*`，再 AgentInfo 标 default 的项），
    /// 和 `session.create` 不传这些字段时一致；model 回到默认而 thinking 没被点名时，thinking 按新模型回落。
    pub fn refill_cleared(&self, id: &str, info: &AgentInfo, s: &mut SessionSettings, clear: &[ClearableSetting]) {
        let wants = |c: ClearableSetting| clear.contains(&c);
        if !(wants(ClearableSetting::Model) || wants(ClearableSetting::Thinking) || wants(ClearableSetting::Mode)) {
            return;
        }
        let filled = self.apply_defaults(
            id,
            info,
            SessionSettings {
                model: s.model.clone(),
                thinking: s.thinking.clone(),
                mode: s.mode.clone(),
                ..Default::default()
            },
        );
        if wants(ClearableSetting::Model) {
            s.model = filled.model;
        }
        if wants(ClearableSetting::Thinking) {
            s.thinking = filled.thinking;
        }
        if wants(ClearableSetting::Mode) {
            s.mode = filled.mode;
        }
        if wants(ClearableSetting::Model) && !wants(ClearableSetting::Thinking) {
            reconcile_thinking(info, s);
        }
    }

    /// `node.set_agent_defaults`：走和 `session.create` 一样的完整校验（含危险模式门禁和选项值），再写 `config.agents.<id>.default_*`。
    pub async fn set_defaults(&self, p: &anycon_protocol::NodeSetAgentDefaultsParams) -> Result<(), RpcError> {
        let info = self
            .info(&p.agent)
            .ok_or_else(|| RpcError::new(ErrorCode::NotFound, format!("unknown agent {}", p.agent)))?;
        let mut checked = SessionSettings {
            model: p.model.clone(),
            thinking: p.thinking.clone(),
            mode: p.mode.clone(),
            options: p.options.clone().unwrap_or_default(),
            ..Default::default()
        };
        // thinking 依赖 model：只给 thinking 时按配置里已有的默认模型 / AgentInfo 默认模型来校验
        if checked.thinking.is_some() && checked.model.is_none() {
            checked.model = self
                .config
                .get()
                .adapter_config(&p.agent)
                .and_then(|c| c.default_model.clone())
                .or_else(|| info.models.iter().find(|m| m.default).map(|m| m.id.clone()));
        }
        self.validate_settings(&p.agent, &info, &checked)?;
        let key = |k: &str| format!("agents.{}.{k}", p.agent);
        let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
        // 一次写入：要么全部生效要么都不写
        let mut edits: Vec<(String, String)> = Vec::new();
        if let Some(m) = &p.model {
            edits.push((key("default_model"), quote(m)));
        }
        if let Some(t) = &p.thinking {
            edits.push((key("default_thinking"), quote(t)));
        }
        if let Some(m) = &p.mode {
            edits.push((key("default_mode"), quote(m)));
        }
        if let Some(o) = &p.options {
            // default_options 是一个表：整体替换
            edits.push((key("default_options"), json_to_toml_inline(&serde_json::Value::Object(o.clone()))));
        }
        let map_err = |e: crate::config::ConfigError| RpcError::new(ErrorCode::Internal, e.to_string());
        // 读-改-写里有 flock 等待和同步文件 IO：放进 blocking 线程
        crate::config::set_values_blocking(self.config.paths(), edits).await.map_err(map_err)?;
        self.config.reload().map_err(map_err)?;
        Ok(())
    }

    // ---------------- 诊断 ----------------

    /// 命令名：优先配置里的 command 第一个词，其次探测到的路径文件名，最后是 agent id。
    fn command_name(&self, id: &str, info: &AgentInfo) -> (String, &'static str) {
        let cfg = self.config.get();
        if let Some(cmd) = cfg.adapter_config(id).and_then(|c| c.command.clone()).filter(|c| !c.trim().is_empty()) {
            let first = shell_words::split(&cmd).ok().and_then(|v| v.into_iter().next()).unwrap_or(cmd);
            return (first, "config");
        }
        if let Some(p) = &info.path {
            if let Some(n) = std::path::Path::new(p).file_name() {
                return (n.to_string_lossy().into_owned(), "PATH");
            }
        }
        (id.to_string(), "default")
    }

    /// `agent.diagnose` / `anycon doctor`：优先用 adapter 自己的诊断（`anycon_adapters::diagnose`），
    /// 没有的退回通用字段：命令来源、daemon PATH、shell、`which -a`、`$SHELL -lc 'type -a'`、解析路径、版本、认证、模型数。
    pub async fn diagnose(&self, id: &str) -> Result<AgentDiagnoseResult, RpcError> {
        let info = self.info(id).ok_or_else(|| RpcError::new(ErrorCode::NotFound, format!("unknown agent {id}")))?;
        let cfg = self.config.get();
        if let Some(ac) = cfg.adapter_config(id) {
            let scratch = self.paths.probe_scratch();
            let _ = std::fs::create_dir_all(&scratch);
            let ctx = ProbeContext {
                env: self.env.for_agent(&cfg, id),
                config: ac.clone(),
                deadline: Duration::from_secs(20),
                cheap: false,
                previous: Some((*info).clone()),
                scratch_dir: scratch,
            };
            if let Ok(Some((text, fields))) =
                tokio::time::timeout(Duration::from_secs(40), anycon_adapters::diagnose(id, &ctx)).await
            {
                self.state.write().unwrap().diagnose.insert(id.to_string(), text.clone());
                warn_err("save the agent cache", self.store.save_agent_cache(&info, Some(&text)).await);
                return Ok(AgentDiagnoseResult { id: id.to_string(), text, fields });
            }
        }
        self.generic_diagnose(id, &info).await
    }

    async fn generic_diagnose(&self, id: &str, info: &AgentInfo) -> Result<AgentDiagnoseResult, RpcError> {
        let (cmd, source) = self.command_name(id, info);
        let path_env = self.env.path();
        let shell = self.env.shell.clone().unwrap_or_else(|| "(unknown)".into());
        let which = run_capture("which", &["-a", &cmd], &self.env.base, Duration::from_secs(5)).await;
        let type_a = if let Some(sh) = &self.env.shell {
            let script = format!("type -a {}", shell_words::quote(&cmd));
            run_capture(sh, &["-lc", &script], &self.env.base, Duration::from_secs(10)).await
        } else {
            "(no $SHELL)".into()
        };
        let auth = match info.status {
            AgentStatus::NotAuthenticated => "not authenticated".to_string(),
            AgentStatus::Ready => "ok".to_string(),
            AgentStatus::Loading => "probe in progress".to_string(),
            _ => info.issue.as_ref().map(|i| i.message.clone()).unwrap_or_else(|| "unknown".into()),
        };
        let fields = vec![
            f("command", &cmd),
            f("command source", source),
            f("daemon PATH", &path_env),
            f("daemon env source", self.env.source.as_str()),
            f("daemon shell", &shell),
            f("which -a", &which),
            f("login shell type -a", &type_a),
            f("resolved path", info.path.as_deref().unwrap_or("(not found)")),
            f("version", info.version.as_deref().unwrap_or("(unknown)")),
            f("auth", &auth),
            f("models", &info.models.len().to_string()),
            f("status", &format!("{:?}", info.status).to_lowercase()),
        ];
        let mut text = format!("{} ({})\n", info.label, info.id);
        for fld in &fields {
            if fld.value.contains('\n') {
                text.push_str(&format!("  {}:\n", fld.label));
                for l in fld.value.lines() {
                    text.push_str(&format!("      {l}\n"));
                }
            } else {
                text.push_str(&format!("  {}: {}\n", fld.label, fld.value));
            }
        }
        if let Some(issue) = &info.issue {
            text.push_str(&format!("  issue: {}: {}\n", issue.code, issue.message));
            if let Some(h) = &issue.hint {
                text.push_str(&format!("  hint: {h}\n"));
            }
        }
        if let Some(w) = &self.env.warning {
            text.push_str(&format!("  warning: {w}\n"));
        }
        self.state.write().unwrap().diagnose.insert(id.to_string(), text.clone());
        warn_err("save the agent cache", self.store.save_agent_cache(info, Some(&text)).await);
        Ok(AgentDiagnoseResult { id: id.to_string(), text, fields })
    }
}

fn f(label: &str, value: &str) -> DiagnosticField {
    DiagnosticField { label: label.to_string(), value: value.to_string() }
}

async fn run_capture(prog: &str, args: &[&str], env: &HashMap<String, String>, timeout: Duration) -> String {
    let mut cmd = Command::new(prog);
    cmd.args(args).env_clear().envs(env.iter()).stdin(std::process::Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                s = String::from_utf8_lossy(&o.stderr).trim().to_string();
            }
            if s.is_empty() {
                format!("(no output, exit {:?})", o.status.code())
            } else {
                s
            }
        }
        Ok(Err(e)) => format!("(failed: {e})"),
        Err(_) => "(timed out)".into(),
    }
}

/// serde_json::Value -> TOML 行内值文本（`config set` 用）。
fn json_to_toml_inline(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "\"\"".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        serde_json::Value::Array(a) => {
            format!("[{}]", a.iter().map(json_to_toml_inline).collect::<Vec<_>>().join(", "))
        }
        serde_json::Value::Object(o) => format!(
            "{{ {} }}",
            o.iter()
                .map(|(k, v)| format!("{} = {}", serde_json::to_string(k).unwrap_or_default(), json_to_toml_inline(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::env::EnvSource;
    use crate::testutil::FakeAdapter;
    use anycon_protocol::RpcNotification;
    use std::sync::atomic::Ordering;

    pub struct Fx {
        pub _d: tempfile::TempDir,
        pub agents: Arc<Agents>,
        pub config: Arc<ConfigStore>,
        pub fake: Arc<FakeAdapter>,
        pub rx: tokio::sync::mpsc::UnboundedReceiver<RpcNotification>,
    }

    async fn fx(cfg_toml: &str) -> Fx {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        std::fs::write(paths.config_toml(), cfg_toml).unwrap();
        let config = ConfigStore::load(&paths).unwrap();
        let store = Store::open(&paths.db()).await.unwrap();
        let clients = Clients::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        clients.register(crate::clients::Caller::local_cli(), Box::new(crate::clients::testing::ChanSink(tx)));
        let env = Arc::new(DaemonEnv {
            base: Arc::new(std::env::vars().collect()),
            source: EnvSource::Process,
            shell: None,
            warning: None,
        });
        let fake = FakeAdapter::new("fake");
        let agents =
            Agents::new(vec![fake.clone() as Arc<dyn Adapter>], config.clone(), env, store, clients.clone(), paths)
                .await;
        Fx { _d: d, agents, config, fake, rx }
    }

    #[tokio::test]
    async fn absent_agents_are_hidden_and_disabled_are_listed() {
        let f = fx("").await;
        assert!(f.agents.list().agents.is_empty());
        let f = fx("[agents.fake]\nenabled = false\n").await;
        let l = f.agents.list();
        assert_eq!(l.agents.len(), 1);
        assert_eq!(l.agents[0].status, AgentStatus::Disabled);
        assert!(!l.agents[0].enabled);
        f.agents.refresh_all().await;
        assert_eq!(f.fake.probe_calls.load(Ordering::SeqCst), 0, "disabled agents are not probed");
    }

    /// 内置的真实 agent（claude-code、codex）没有 `[agents.<id>]` 段也参与探测；`mock` 没有段就不出现；
    /// `enabled = false` 才关（daemon.md § 5.6）。
    #[tokio::test]
    async fn builtin_agents_are_probed_without_a_config_section_and_mock_is_not() {
        let build = |cfg_toml: &'static str| async move {
            let d = tempfile::tempdir().unwrap();
            let paths = Paths::new(d.path());
            std::fs::write(paths.config_toml(), cfg_toml).unwrap();
            let config = ConfigStore::load(&paths).unwrap();
            let store = Store::open(&paths.db()).await.unwrap();
            let env = Arc::new(DaemonEnv {
                base: Arc::new(std::env::vars().collect()),
                source: EnvSource::Process,
                shell: None,
                warning: None,
            });
            let fakes = [FakeAdapter::new("claude-code"), FakeAdapter::new("codex"), FakeAdapter::new("mock")];
            let adapters: Vec<Arc<dyn Adapter>> = fakes.iter().map(|f| f.clone() as Arc<dyn Adapter>).collect();
            let agents = Agents::new(adapters, config.clone(), env, store, Clients::new(), paths).await;
            (d, agents, config, fakes)
        };
        let (_d, agents, _config, fakes) = build("").await;
        agents.refresh_all().await;
        let l = agents.list();
        let ids: Vec<&str> = l.agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-code", "codex"], "no section needed; mock stays hidden");
        assert!(l.agents.iter().all(|a| a.status == AgentStatus::Ready && a.enabled), "{:?}", l.agents);
        assert_eq!(fakes[2].probe_calls.load(Ordering::SeqCst), 0, "mock is never probed without its section");

        // `enabled = false` 关掉其中一个：仍然列出（disabled），不探测
        let (_d, agents, config, fakes) = build("[agents.codex]\nenabled = false\n").await;
        agents.refresh_all().await;
        let by = |id: &str| agents.info(id).unwrap().status;
        assert_eq!((by("claude-code"), by("codex")), (AgentStatus::Ready, AgentStatus::Disabled));
        assert_eq!(fakes[1].probe_calls.load(Ordering::SeqCst), 0);

        // 热加载：运行中把 claude-code 关掉、把 codex 打开，列表跟着变
        crate::config::set_value(config.paths(), "agents.claude-code.enabled", "false").unwrap();
        crate::config::set_value(config.paths(), "agents.codex.enabled", "true").unwrap();
        config.reload().unwrap();
        agents.on_config_changed().await;
        let t = std::time::Instant::now();
        while (by("claude-code"), by("codex")) != (AgentStatus::Disabled, AgentStatus::Ready) {
            assert!(t.elapsed() < Duration::from_secs(5), "{:?} {:?}", by("claude-code"), by("codex"));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn loading_then_ready_etag_and_changed_notification() {
        let mut f = fx("[agents.fake]\nenabled = true\n").await;
        assert_eq!(f.agents.list().agents[0].status, AgentStatus::Loading);
        let etag0 = f.agents.etag();
        let r = f.agents.handle_list(AgentListParams { refresh: true, if_none_match: None }).await;
        assert_eq!(r.agents[0].status, AgentStatus::Ready);
        assert_ne!(r.etag, etag0);
        assert!(!r.not_modified);
        let n = f.rx.try_recv().expect("agent.changed pushed");
        assert_eq!(n.method, methods::AGENT_CHANGED);
        // if_none_match 命中
        let r2 = f.agents.handle_list(AgentListParams { refresh: false, if_none_match: Some(r.etag.clone()) }).await;
        assert!(r2.not_modified && r2.agents.is_empty());
        // 内容没变的重探：etag 不变，不再推送
        f.agents.probe("fake", false).await;
        assert_eq!(f.agents.etag(), r.etag);
        assert!(f.rx.try_recv().is_err());
        // 装了新版本：etag 变，推送
        f.fake.set_version("2.0.0");
        f.agents.probe("fake", false).await;
        assert_ne!(f.agents.etag(), r.etag);
        assert_eq!(f.rx.try_recv().unwrap().method, methods::AGENT_CHANGED);
    }

    #[tokio::test]
    async fn cheap_probes_skip_model_step_and_version_change_upgrades() {
        let f = fx("[agents.fake]\nenabled = true\n").await;
        f.agents.probe("fake", false).await; // 首次：完整
        assert_eq!(f.fake.full_probes.load(Ordering::SeqCst), 1);
        f.agents.probe("fake", true).await;
        f.agents.probe("fake", true).await;
        assert_eq!(f.fake.full_probes.load(Ordering::SeqCst), 1, "periodic probes are cheap");
        assert_eq!(f.fake.probe_calls.load(Ordering::SeqCst), 3);
        // cheap 探测发现版本变了：自动补一次完整探测
        f.fake.set_version("3.0.0");
        f.agents.probe("fake", true).await;
        assert_eq!(f.fake.full_probes.load(Ordering::SeqCst), 2);
        assert_eq!(f.agents.info("fake").unwrap().version.as_deref(), Some("3.0.0"));
    }

    #[tokio::test]
    async fn one_probe_at_a_time_and_overall_timeout() {
        let f = fx("[agents]\nprobe_timeout = \"1s\"\n[agents.fake]\nenabled = true\n").await;
        f.fake.set_probe_delay(Duration::from_millis(400));
        let a = f.agents.clone();
        let b = f.agents.clone();
        let (r1, r2) = tokio::join!(a.probe("fake", false), b.probe("fake", false));
        assert!(r1.is_some() && r2.is_some());
        assert_eq!(
            f.fake.max_concurrent_probes.load(Ordering::SeqCst),
            1,
            "never two probes of the same agent at once"
        );
        // 总超时：adapter 卡住 => probe_timeout 错误
        f.fake.set_probe_delay(Duration::from_secs(30));
        let t = std::time::Instant::now();
        let info = f.agents.probe("fake", false).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(6));
        assert_eq!(info.status, AgentStatus::Error);
        assert_eq!(info.issue.as_ref().unwrap().code, "probe_timeout");
    }

    #[tokio::test]
    async fn config_changes_trigger_reprobe_and_enable_toggle() {
        let f = fx("[agents.fake]\nenabled = true\n").await;
        let _sched = f.agents.spawn_scheduler();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(f.agents.info("fake").unwrap().status, AgentStatus::Ready);
        let before = f.fake.probe_calls.load(Ordering::SeqCst);
        // 只改 default_*：不重探
        crate::config::set_value(f.config.paths(), "agents.fake.default_model", "\"fake-small\"").unwrap();
        f.config.reload().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(f.fake.probe_calls.load(Ordering::SeqCst), before);
        // 关掉
        crate::config::set_value(f.config.paths(), "agents.fake.enabled", "false").unwrap();
        f.config.reload().unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(f.agents.info("fake").unwrap().status, AgentStatus::Disabled);
        // 再打开：重新完整探测
        crate::config::set_value(f.config.paths(), "agents.fake.enabled", "true").unwrap();
        f.config.reload().unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(f.agents.info("fake").unwrap().status, AgentStatus::Ready);
        // 改 command：重探
        let calls = f.fake.probe_calls.load(Ordering::SeqCst);
        crate::config::set_value(f.config.paths(), "agents.fake.command", "\"/opt/fake\"").unwrap();
        f.config.reload().unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(f.fake.probe_calls.load(Ordering::SeqCst) > calls);
    }

    #[tokio::test]
    async fn defaults_validation_dangerous_gate_and_merge() {
        let f = fx("[agents.fake]\nenabled = true\ndefault_model = \"fake-small\"\ndefault_mode = \"nope\"\n").await;
        f.agents.probe("fake", false).await;
        let info = f.agents.info("fake").unwrap();
        // 配置里的 default_model 有效，default_mode 无效 -> 回落到 AgentInfo 的 default
        let s = f.agents.apply_defaults("fake", &info, SessionSettings::default());
        assert_eq!(s.model.as_deref(), Some("fake-small"));
        assert_eq!(s.thinking, None, "fake-small has no thinking options");
        assert_eq!(s.mode.as_deref(), Some("ask"));
        // 默认模型是 large 时 thinking 取模型默认
        let s = f.agents.apply_defaults(
            "fake",
            &info,
            SessionSettings { model: Some("fake-large".into()), ..Default::default() },
        );
        assert_eq!(s.thinking.as_deref(), Some("high"));
        f.agents.validate_settings("fake", &info, &s).unwrap();
        // 非法 thinking
        let bad =
            SessionSettings { model: Some("fake-large".into()), thinking: Some("max".into()), ..Default::default() };
        let e = f.agents.validate_settings("fake", &info, &bad).unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::InvalidSettings));
        assert_eq!(e.data.unwrap().field.as_deref(), Some("thinking"));
        // 会话 env 默认关闭：任何非空 env 都是 invalid_settings { field: "env" }
        let mut env_ok = SessionSettings::default();
        env_ok.env.insert("FOO".into(), "bar".into());
        let e = f.agents.validate_settings("fake", &info, &env_ok).unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::InvalidSettings));
        assert_eq!(e.data.unwrap().field.as_deref(), Some("env"));
        // 打开之后：合规的键通过，受保护 / 黑名单的键得到 env.<KEY>
        crate::config::set_value(f.config.paths(), "permissions.allow_session_env", "true").unwrap();
        f.config.reload().unwrap();
        // 开关打开但允许列表为空：一个键都不行（reason 指向 permissions.session_env_allow）
        let e = f.agents.validate_settings("fake", &info, &env_ok).unwrap_err();
        assert_eq!(e.data.unwrap().field.as_deref(), Some("env.FOO"));
        crate::config::set_value(
            f.config.paths(),
            "permissions.session_env_allow",
            r#"["FOO", "PATH", "HOME", "BASH_ENV", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "NODE_OPTIONS", "ANTHROPIC_BASE_URL", "CLAUDE_CONFIG_DIR", "ANYCON_*", "HTTPS_PROXY", "GIT_SSH_COMMAND", "ZDOTDIR", "PYTHONPATH"]"#,
        )
        .unwrap();
        f.config.reload().unwrap();
        f.agents.validate_settings("fake", &info, &env_ok).unwrap();
        for key in [
            "PATH",
            "HOME",
            "BASH_ENV",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "NODE_OPTIONS",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CONFIG_DIR",
            "ANYCON_X",
            "HTTPS_PROXY",
            "GIT_SSH_COMMAND",
            "ZDOTDIR",
            "PYTHONPATH",
        ] {
            let mut bad = SessionSettings::default();
            bad.env.insert(key.into(), "/x".into());
            let e = f.agents.validate_settings("fake", &info, &bad).unwrap_err();
            assert_eq!(e.stable_code(), Some(ErrorCode::InvalidSettings), "{key}");
            assert_eq!(e.data.unwrap().field.as_deref(), Some(format!("env.{key}").as_str()), "{key}");
        }
        // 危险模式门禁
        let yolo = SessionSettings { mode: Some("yolo".into()), ..Default::default() };
        f.agents.validate_settings("fake", &info, &yolo).unwrap();
        crate::config::set_value(f.config.paths(), "permissions.allow_dangerous_modes", "false").unwrap();
        f.config.reload().unwrap();
        assert_eq!(
            f.agents.validate_settings("fake", &info, &yolo).unwrap_err().stable_code(),
            Some(ErrorCode::Forbidden)
        );
        assert!(f.agents.is_dangerous_mode("fake", &info, "yolo"));
        // 合并：只改 model -> thinking 回落
        let old =
            SessionSettings { model: Some("fake-large".into()), thinking: Some("low".into()), ..Default::default() };
        let (m, rec) = Agents::merge_settings(
            &info,
            &old,
            &SessionSettings { model: Some("fake-small".into()), ..Default::default() },
            &[],
        );
        assert!(rec);
        assert_eq!(m.model.as_deref(), Some("fake-small"));
        assert_eq!(m.thinking, None);
        let (m, rec) = Agents::merge_settings(
            &info,
            &old,
            &SessionSettings { thinking: Some("high".into()), ..Default::default() },
            &[],
        );
        assert!(!rec);
        assert_eq!(m.thinking.as_deref(), Some("high"));
        // 同时给了 model 和 thinking：不回落（交给校验）
        let (m, rec) = Agents::merge_settings(
            &info,
            &old,
            &SessionSettings { model: Some("fake-small".into()), thinking: Some("low".into()), ..Default::default() },
            &[],
        );
        assert!(!rec);
        assert_eq!(m.thinking.as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn set_defaults_runs_the_full_validation_including_the_dangerous_gate() {
        use anycon_protocol::NodeSetAgentDefaultsParams as P;
        let f = fx("[permissions]\nallow_dangerous_modes = false\n[agents.fake]\nenabled = true\n").await;
        f.agents.probe("fake", false).await;
        let p = |mode: Option<&str>, options: Option<serde_json::Value>| P {
            agent: "fake".into(),
            model: None,
            thinking: None,
            mode: mode.map(str::to_string),
            options: options.map(|o| o.as_object().unwrap().clone()),
        };
        // 危险模式被禁用：不能被写成默认（以前 set_agent_defaults 绕过了门禁）
        let e = f.agents.set_defaults(&p(Some("yolo"), None)).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
        assert!(f.config.get().adapter_config("fake").unwrap().default_mode.is_none(), "nothing was written");
        // 选项值要符合声明的类型（verbose 是 bool）
        let e = f.agents.set_defaults(&p(None, Some(serde_json::json!({ "verbose": "yes" })))).await.unwrap_err();
        assert_eq!(e.stable_code(), Some(ErrorCode::InvalidSettings));
        assert_eq!(e.data.unwrap().field.as_deref(), Some("options.verbose"));
        // 未知的模式 / 选项
        assert!(f.agents.set_defaults(&p(Some("nope"), None)).await.is_err());
        assert!(f.agents.set_defaults(&p(None, Some(serde_json::json!({ "zzz": true })))).await.is_err());
        // 合法的值写入；多个键一次写完
        let mut ok = p(Some("plan"), Some(serde_json::json!({ "verbose": true })));
        ok.model = Some("fake-large".into());
        ok.thinking = Some("low".into());
        f.agents.set_defaults(&ok).await.unwrap();
        let cfg = f.config.get();
        let ac = cfg.adapter_config("fake").unwrap();
        assert_eq!(ac.default_model.as_deref(), Some("fake-large"));
        assert_eq!(ac.default_thinking.as_deref(), Some("low"));
        assert_eq!(ac.default_mode.as_deref(), Some("plan"));
        assert_eq!(ac.default_options.get("verbose"), Some(&serde_json::json!(true)));
        // 允许危险模式之后才能写
        crate::config::set_value(f.config.paths(), "permissions.allow_dangerous_modes", "true").unwrap();
        f.config.reload().unwrap();
        f.agents.set_defaults(&p(Some("yolo"), None)).await.unwrap();
    }

    #[test]
    fn merge_settings_clear_list_resets_named_fields_after_the_merge() {
        let info = AgentInfo {
            id: "x".into(),
            label: "x".into(),
            driver: "x".into(),
            enabled: true,
            status: AgentStatus::Ready,
            version: None,
            path: None,
            issue: None,
            models: vec![],
            allow_custom_model: true,
            modes: vec![],
            options: vec![],
            capabilities: Default::default(),
            probed_at_ms: 0,
        };
        let mut old = SessionSettings {
            system_prompt_append: Some("be brief".into()),
            max_turns: Some(5),
            max_budget_usd: Some(2.5),
            ..Default::default()
        };
        old.env.insert("A".into(), "1".into());
        old.env.insert("B".into(), "2".into());
        old.options.insert("verbose".into(), serde_json::json!(true));
        // settings 缺省 / null = 保持
        let (m, _) = Agents::merge_settings(&info, &old, &SessionSettings::default(), &[]);
        assert_eq!(m, old);
        // clear 点名的字段恢复为未设置，别的不动
        let (m, _) = Agents::merge_settings(
            &info,
            &old,
            &SessionSettings::default(),
            &[ClearableSetting::SystemPromptAppend, ClearableSetting::MaxTurns, ClearableSetting::Env],
        );
        assert_eq!((m.system_prompt_append, m.max_turns), (None, None));
        assert_eq!(m.max_budget_usd, Some(2.5));
        assert!(m.env.is_empty());
        assert_eq!(m.options.len(), 1);
        // 同时在 settings 和 clear 里出现：以 clear 为准；options 里 null 删键
        let mut both = SessionSettings { max_budget_usd: Some(9.0), ..Default::default() };
        both.options.insert("verbose".into(), serde_json::Value::Null);
        let (m, _) = Agents::merge_settings(&info, &old, &both, &[ClearableSetting::MaxBudgetUsd]);
        assert_eq!(m.max_budget_usd, None);
        assert!(m.options.is_empty());
    }

    #[tokio::test]
    async fn clearing_model_thinking_and_mode_falls_back_to_the_defaults() {
        let f = fx("[agents.fake]\nenabled = true\ndefault_model = \"fake-large\"\n").await;
        f.agents.probe("fake", false).await;
        let info = f.agents.info("fake").unwrap();
        let old = SessionSettings {
            model: Some("fake-small".into()),
            thinking: None,
            mode: Some("plan".into()),
            ..Default::default()
        };
        let clear = [ClearableSetting::Model, ClearableSetting::Mode];
        let (mut m, _) = Agents::merge_settings(&info, &old, &SessionSettings::default(), &clear);
        assert_eq!((m.model.clone(), m.mode.clone()), (None, None));
        f.agents.refill_cleared("fake", &info, &mut m, &clear);
        assert_eq!(m.model.as_deref(), Some("fake-large"), "the configured default model");
        assert_eq!(m.mode.as_deref(), Some("ask"), "the AgentInfo default mode");
        // 模型回到默认而 thinking 没点名：thinking 按新模型回落（这里 fake-large 没有被设置过的 thinking -> 保持 None 或默认）
        f.agents.validate_settings("fake", &info, &m).unwrap();
        // 只清 thinking：回到当前模型的默认
        let old =
            SessionSettings { model: Some("fake-large".into()), thinking: Some("low".into()), ..Default::default() };
        let clear = [ClearableSetting::Thinking];
        let (mut m, _) = Agents::merge_settings(&info, &old, &SessionSettings::default(), &clear);
        f.agents.refill_cleared("fake", &info, &mut m, &clear);
        assert_eq!(m.thinking.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn client_view_reflects_defaults_and_hides_forbidden_modes() {
        let mut f = fx(
            "[permissions]\nallow_dangerous_modes = false\n[agents.fake]\nenabled = true\ndefault_model = \"fake-small\"\ndefault_mode = \"plan\"\ndefault_thinking = \"low\"\n[agents.fake.default_options]\nverbose = true\n",
        )
        .await;
        f.agents.probe("fake", false).await;
        let _ = f.rx.try_recv();
        let l = f.agents.list();
        let a = &l.agents[0];
        let model = |id: &str| a.models.iter().find(|m| m.id == id).unwrap();
        assert!(model("fake-small").default && !model("fake-large").default, "the node default model is flagged");
        assert_eq!(model("fake-large").thinking.as_ref().unwrap().default.as_deref(), Some("low"));
        assert_eq!(
            a.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["ask", "plan"],
            "dangerous modes are hidden when forbidden"
        );
        assert!(
            a.modes.iter().find(|m| m.id == "plan").unwrap().default
                && !a.modes.iter().find(|m| m.id == "ask").unwrap().default
        );
        assert_eq!(a.options[0].default, serde_json::json!(true));
        // 探测结果本身不被改写：校验和危险模式门禁用完整信息
        assert!(f.agents.info("fake").unwrap().modes.iter().any(|m| m.id == "yolo"));
        let etag = l.etag.clone();
        // 配置变化（调度器在跑）-> etag 变，并向客户端推 agent.changed
        let _sched = f.agents.spawn_scheduler();
        tokio::time::sleep(Duration::from_millis(300)).await;
        while f.rx.try_recv().is_ok() {}
        crate::config::set_value(f.config.paths(), "permissions.allow_dangerous_modes", "true").unwrap();
        f.config.reload().unwrap();
        let n = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let n = f.rx.recv().await.unwrap();
                if n.method == methods::AGENT_CHANGED {
                    break n;
                }
            }
        })
        .await
        .expect("agent.changed after the config change");
        let pushed: AgentListResult = serde_json::from_value(n.params).unwrap();
        assert_ne!(pushed.etag, etag);
        assert!(pushed.agents[0].modes.iter().any(|m| m.id == "yolo"));
        // dangerous_modes 把模式标成 dangerous
        crate::config::set_value(f.config.paths(), "agents.fake.dangerous_modes", "[\"plan\"]").unwrap();
        f.config.reload().unwrap();
        let l = f.agents.list();
        assert_eq!(l.agents[0].modes.iter().find(|m| m.id == "plan").unwrap().risk, ModeRisk::Dangerous);
    }

    #[tokio::test]
    async fn set_defaults_writes_config() {
        let f = fx("[agents.fake]\nenabled = true\n").await;
        f.agents.probe("fake", false).await;
        let ok = anycon_protocol::NodeSetAgentDefaultsParams {
            agent: "fake".into(),
            model: Some("fake-large".into()),
            thinking: Some("low".into()),
            mode: Some("plan".into()),
            options: None,
        };
        f.agents.set_defaults(&ok).await.unwrap();
        let cfg = f.config.get();
        let ac = cfg.adapter_config("fake").unwrap();
        assert_eq!(ac.default_model.as_deref(), Some("fake-large"));
        assert_eq!(ac.default_thinking.as_deref(), Some("low"));
        assert_eq!(ac.default_mode.as_deref(), Some("plan"));
        let bad = anycon_protocol::NodeSetAgentDefaultsParams {
            agent: "fake".into(),
            model: Some("nonexistent".into()),
            thinking: None,
            mode: None,
            options: None,
        };
        assert_eq!(f.agents.set_defaults(&bad).await.unwrap_err().stable_code(), Some(ErrorCode::InvalidSettings));
        let mut opts = serde_json::Map::new();
        opts.insert("verbose".into(), serde_json::json!(true));
        f.agents
            .set_defaults(&anycon_protocol::NodeSetAgentDefaultsParams {
                agent: "fake".into(),
                model: None,
                thinking: None,
                mode: None,
                options: Some(opts),
            })
            .await
            .unwrap();
        assert_eq!(f.config.get().adapter_config("fake").unwrap().default_options["verbose"], serde_json::json!(true));
        let unknown = anycon_protocol::NodeSetAgentDefaultsParams {
            agent: "ghost".into(),
            model: None,
            thinking: None,
            mode: None,
            options: None,
        };
        assert_eq!(f.agents.set_defaults(&unknown).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn diagnose_has_expected_fields() {
        let f = fx("[agents.fake]\nenabled = true\ncommand = \"sh -c x\"\n").await;
        f.agents.probe("fake", false).await;
        let d = f.agents.diagnose("fake").await.unwrap();
        let labels: Vec<&str> = d.fields.iter().map(|x| x.label.as_str()).collect();
        for want in [
            "command",
            "command source",
            "daemon PATH",
            "daemon shell",
            "which -a",
            "resolved path",
            "version",
            "auth",
            "models",
            "status",
        ] {
            assert!(labels.contains(&want), "missing {want}: {labels:?}");
        }
        assert!(d.text.contains("command: sh"));
        assert_eq!(f.agents.diagnose("ghost").await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
        let _ = Config::default();
    }
}
