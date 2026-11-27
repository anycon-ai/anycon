//! 客户端设备在本机上的状态：每个 hub 一套，存在 `<home>/client/<hub-host>/`（CLI 和 Twin 设备共用同一套格式）：
//! `client.key`（Noise 私钥 hex，0600）、`device-token`（0600）、`device-token-issued-at`（token 的签发时间，Unix 毫秒）、
//! `device-id`、`nodes.json`（已配对节点：id、名称、公钥）。
//!
//! 文件格式和位置从 CLI 沿用，升级前后互相可读；写入都是「临时文件 + rename」的原子替换，token 先于签发时间落盘
//! （见 [`write_token_files`]）。

use std::path::{Path, PathBuf};

use anycon_crypto::Keypair;
use anycon_protocol::{ClientInfo, DeviceKind};
use serde::{Deserialize, Serialize};

use super::error::{ClientError, Result};
use crate::config::Paths;
use crate::hub::platform;

/// 节点名最多显示 / 保存多少个字符。
pub(crate) const NAME_MAX_CHARS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeEntry {
    pub id: String,
    pub name: String,
    /// 节点静态公钥 hex
    pub pubkey: String,
    pub paired_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct NodesFile {
    hub_url: String,
    nodes: Vec<NodeEntry>,
}

/// 一个 hub 对应的本地客户端状态。
pub struct HubState {
    pub dir: PathBuf,
    pub hub_url: String,
    pub secret: Option<[u8; 32]>,
    pub token: Option<String>,
    /// `token` 的签发时间（Unix 毫秒）；没有记录（轮换功能之前配对的）= 未知，按「该轮换了」处理。
    pub token_issued_at_ms: Option<u64>,
    pub device_id: Option<String>,
    pub nodes: Vec<NodeEntry>,
    /// 状态文件存在但读不出来（损坏）：不能当作「没有」继续——`connect` 会生成新密钥把旧的覆盖掉。
    pub problem: Option<String>,
    /// 载入时磁盘上的 token。`save` 只在 `token` 变了（配对拿到新的）才写 token 文件：一份在轮换之前载入的旧状态
    /// 之后 `save`（改节点名、`disconnect`）不会把轮换好的新 token 盖回旧的。
    loaded_token: Option<String>,
}

/// 签发时间文件的名字（和 `device-token` 在同一个目录）。
pub(crate) const TOKEN_ISSUED_FILE: &str = "device-token-issued-at";

/// 把 token 和它的签发时间写进状态目录：**先 token、后签发时间**，各自是「临时文件 + rename」的原子替换（0600）。
/// 中途崩溃只会留下「新 token + 旧签发时间」（下次多轮换一次），不会留下「新签发时间 + 旧 token」
/// （那会让一个已被 hub 换掉的 token 被当成新的，宽限一过就认证失败）。
pub(crate) fn write_token_files(dir: &Path, token: &str, issued_at_ms: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let _ = crate::sys::restrict_to_owner(dir);
    crate::identity::write_private(&dir.join("device-token"), token)?;
    crate::identity::write_private(&dir.join(TOKEN_ISSUED_FILE), &issued_at_ms.to_string())
}

pub fn host_of(hub_url: &str) -> Result<String> {
    let (_, origin) = crate::hub::ws_url(hub_url).map_err(ClientError::usage)?;
    Ok(origin.split_once("://").map(|x| x.1).unwrap_or(&origin).to_string())
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl HubState {
    /// 读某个目录里的状态（目录不存在 = 空状态）。文件存在但损坏时记在 `problem` 里，由调用方决定怎么处理。
    pub fn load(dir: &Path, hub_url: &str) -> HubState {
        let mut problems = Vec::new();
        let nodes_file: NodesFile = match std::fs::read_to_string(dir.join("nodes.json")) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
                problems.push(format!("{} is corrupt ({e})", dir.join("nodes.json").display()));
                NodesFile::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => NodesFile::default(),
            Err(e) => {
                problems.push(format!("cannot read {} ({e})", dir.join("nodes.json").display()));
                NodesFile::default()
            }
        };
        let key_path = dir.join("client.key");
        let secret = match read_trim(&key_path) {
            Some(h) => match hex::decode(h).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()) {
                Some(s) => Some(s),
                None => {
                    problems.push(format!("{} is corrupt", key_path.display()));
                    None
                }
            },
            None if key_path.exists() => {
                problems.push(format!("{} is empty", key_path.display()));
                None
            }
            None => None,
        };
        let token = read_trim(&dir.join("device-token"));
        let token_issued_at_ms =
            token.as_ref().and_then(|_| crate::identity::read_issued_at(&dir.join(TOKEN_ISSUED_FILE)));
        HubState {
            dir: dir.to_path_buf(),
            hub_url: if nodes_file.hub_url.is_empty() { hub_url.to_string() } else { nodes_file.hub_url },
            secret,
            loaded_token: token.clone(),
            token,
            token_issued_at_ms,
            device_id: read_trim(&dir.join("device-id")),
            nodes: nodes_file.nodes,
            problem: (!problems.is_empty()).then(|| problems.join("; ")),
        }
    }

    /// 状态损坏时的错误（修好或移走文件之后才能继续）。
    pub fn check(&self) -> Result<()> {
        match &self.problem {
            Some(p) => Err(ClientError::general(format!(
                "the saved client state for {} is unusable: {p}. Fix or move the files in {} (nothing was overwritten)",
                self.hub_url,
                self.dir.display()
            ))),
            None => Ok(()),
        }
    }

    pub fn keypair(&self) -> Option<Keypair> {
        self.secret.map(Keypair::from_secret)
    }

    fn write_private(&self, name: &str, content: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let _ = crate::sys::restrict_to_owner(&self.dir);
        crate::identity::write_private(&self.dir.join(name), content)?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.check()?;
        if let Some(s) = &self.secret {
            self.write_private("client.key", &hex::encode(s))?;
        }
        if let Some(t) = self.token.as_ref().filter(|t| self.loaded_token.as_ref() != Some(*t)) {
            // 配对刚拿到的新 token（它的签发时间是此刻）；没变的 token 不碰，见 `loaded_token`
            let issued = self.token_issued_at_ms.unwrap_or_else(crate::util::now_ms);
            write_token_files(&self.dir, t, issued)?;
        }
        if let Some(i) = &self.device_id {
            self.write_private("device-id", i)?;
        }
        let f = NodesFile { hub_url: self.hub_url.clone(), nodes: self.nodes.clone() };
        self.write_private("nodes.json", &serde_json::to_string_pretty(&f)?)?;
        Ok(())
    }

    /// hub 轮换了设备 token：内存里的状态跟着换（磁盘已由调用方写好），之后的 `save` 不会把它盖回旧的。
    pub(crate) fn adopt_rotated_token(&mut self, token: &str) {
        self.token = Some(token.to_string());
        self.loaded_token = self.token.clone();
        self.token_issued_at_ms = Some(crate::util::now_ms());
    }

    /// hub 不认这个设备了（token 无效 / 已被吊销）：丢掉保存的 token 和设备 id，之后以临时连接重新配对。
    pub fn forget_credentials(&mut self) -> Result<()> {
        self.token = None;
        self.token_issued_at_ms = None;
        self.loaded_token = None;
        self.device_id = None;
        for f in ["device-token", TOKEN_ISSUED_FILE, "device-id"] {
            match std::fs::remove_file(self.dir.join(f)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn find_node(&self, key: &str) -> Option<&NodeEntry> {
        self.nodes.iter().find(|n| n.id == key).or_else(|| {
            let by_name: Vec<&NodeEntry> = self.nodes.iter().filter(|n| n.name == key).collect();
            if by_name.len() == 1 {
                return Some(by_name[0]);
            }
            let by_prefix: Vec<&NodeEntry> =
                self.nodes.iter().filter(|n| !key.is_empty() && n.id.starts_with(key)).collect();
            (by_prefix.len() == 1).then(|| by_prefix[0])
        })
    }
}

/// 列出本机上所有 hub 的客户端状态。
pub fn list_hubs(paths: &Paths) -> Vec<HubState> {
    let root = paths.home.join("client");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                let st = HubState::load(&e.path(), "");
                if !st.hub_url.is_empty() || st.problem.is_some() {
                    out.push(st);
                }
            }
        }
    }
    out.sort_by(|a, b| a.hub_url.cmp(&b.hub_url));
    out
}

/// 每次连上节点之后用 `node.info` 刷新本地保存的节点名（best effort；名字经过清理）。
pub fn refresh_node_name(state: &mut HubState, node_id: &str, name: &str) {
    let clean = sanitize_label(name, NAME_MAX_CHARS, "");
    if clean.is_empty() {
        return;
    }
    if let Some(n) = state.nodes.iter_mut().find(|n| n.id == node_id) {
        if n.name != clean {
            n.name = clean;
            if let Err(e) = state.save() {
                tracing::debug!(error = %e, "could not refresh the saved node name");
            }
        }
    }
}

/// 不可见的格式字符：Unicode 的 Format 类（双向文字控制符、零宽字符、BOM、U+061C……），加上 Tag 字符 U+E0000–E007F、
/// 行 / 段分隔符 U+2028/2029、组合用字素连接符 U+034F、盲文空白 U+2800、Hangul 填充符（U+115F/1160/3164/FFA0）、
/// 蒙古文自由变体选择符 U+180B–180F、变体选择符（U+FE00–FE0F、U+E0100–E01EF）。
/// 它们不会被 `char::is_control` 抓到，却能让终端上显示的和实际的内容不一样（视觉欺骗）。
pub fn is_invisible_format(c: char) -> bool {
    use unicode_general_category::{get_general_category, GeneralCategory};
    get_general_category(c) == GeneralCategory::Format
        || matches!(
            c,
            '\u{2028}'
                | '\u{2029}'
                | '\u{034F}'
                | '\u{2800}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{3164}'
                | '\u{FFA0}'
                | '\u{180B}'..='\u{180F}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

/// 来自对端的、要显示或存起来的名字：去掉控制字符和不可见的格式字符（双向文字控制符、零宽字符、U+2028/2029、BOM、
/// U+061C…，防终端转义序列 / 视觉欺骗），修剪空白，按**字符**截断到 `max` 个字符；清理后为空就用 `fallback`。
pub fn sanitize_label(s: &str, max: usize, fallback: &str) -> String {
    let cleaned: String = s.chars().filter(|c| !c.is_control() && !is_invisible_format(*c)).collect();
    let t: String = cleaned.trim().chars().take(max).collect();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t
    }
}

/// 本设备在 hub 上自报的信息；`kind` 由调用方给（CLI 是 `cli`，Twin 设备是 `twin`）。
pub fn client_info(name: &str, kind: DeviceKind) -> ClientInfo {
    ClientInfo {
        name: name.to_string(),
        kind,
        platform: platform(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        origin: None,
    }
}
