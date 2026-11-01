//! 线上格式里我们用到的那一小部分（ACP v1 的 `initialize` 响应）。
//!
//! 按官方 schema（`schema/v1/schema.json`）的字段名，**宽松解析**：schema 里一大批字段标了
//! `x-deserialize-default-on-error`（接收方对写坏的值要退回默认值，而不是整条消息失败），所以这里每个字段
//! 都用 [`lenient`]：类型不对就当没有。未知字段忽略。`session/new` 等响应里的 `modes` / `configOptions` 由 `options.rs`
//! 直接从 JSON 里取。

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// 类型不对（包括 `null` 以外的任何不合法值）就退回 `Default`。
pub fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    #[serde(default, deserialize_with = "lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "lenient")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub version: String,
}

impl Implementation {
    /// 给人看的名字：`title`，没有就用 `name`。
    pub fn display(&self) -> Option<&str> {
        self.title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .or(Some(self.name.as_str()).filter(|n| !n.trim().is_empty()))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCaps {
    #[serde(default, deserialize_with = "lenient")]
    pub image: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub audio: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub embedded_context: bool,
}

/// `sessionCapabilities`：每一项是对象（含 `{}`）表示支持，缺省或 `null` 表示不支持。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCaps {
    #[serde(default, deserialize_with = "lenient")]
    pub resume: Option<Value>,
    #[serde(default, deserialize_with = "lenient")]
    pub close: Option<Value>,
    #[serde(default, deserialize_with = "lenient")]
    pub list: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCaps {
    /// `session/load`（回放全部历史）。
    #[serde(default, deserialize_with = "lenient")]
    pub load_session: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub prompt_capabilities: PromptCaps,
    #[serde(default, deserialize_with = "lenient")]
    pub session_capabilities: SessionCaps,
}

impl AgentCaps {
    pub fn can_resume(&self) -> bool {
        self.session_capabilities.resume.is_some()
    }

    pub fn can_close(&self) -> bool {
        self.session_capabilities.close.is_some()
    }

    /// 进程退出后能不能接上原来的会话：`session/resume` 或 `session/load` 有一个就行。
    pub fn can_restore(&self) -> bool {
        self.can_resume() || self.load_session
    }
}

/// 登录方式。`type` 缺省 = `agent`（agent 自己处理，经 `authenticate`）；`terminal` = 用户要在机器上跑一次命令。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthMethod {
    #[serde(default, deserialize_with = "lenient")]
    pub id: String,
    #[serde(default, deserialize_with = "lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "lenient")]
    pub description: Option<String>,
    #[serde(default, rename = "type", deserialize_with = "lenient")]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub args: Vec<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub env: HashMap<String, String>,
}

impl AuthMethod {
    pub fn is_terminal(&self) -> bool {
        self.kind.as_deref() == Some("terminal")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    #[serde(default, deserialize_with = "lenient")]
    pub protocol_version: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    pub agent_capabilities: AgentCaps,
    #[serde(default, deserialize_with = "lenient")]
    pub auth_methods: Vec<AuthMethod>,
    #[serde(default, deserialize_with = "lenient")]
    pub agent_info: Option<Implementation>,
}

impl InitializeResult {
    pub fn from_value(v: &Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_typical_initialize_response() {
        let r = InitializeResult::from_value(&json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true },
                "sessionCapabilities": { "resume": {}, "close": {}, "list": null },
                "mcpCapabilities": { "http": true }
            },
            "authMethods": [
                { "id": "login", "name": "Log in", "description": "Sign in" },
                { "id": "t", "name": "Terminal", "type": "terminal", "args": ["--login"], "env": { "A": "b" } }
            ],
            "agentInfo": { "name": "my-agent", "title": "My Agent", "version": "1.2.3" }
        }));
        assert_eq!(r.protocol_version, Some(1));
        let c = &r.agent_capabilities;
        assert!(c.load_session && c.prompt_capabilities.image && c.prompt_capabilities.embedded_context);
        assert!(!c.prompt_capabilities.audio);
        assert!(c.can_resume() && c.can_close() && c.can_restore());
        assert!(c.session_capabilities.list.is_none());
        assert_eq!(r.auth_methods.len(), 2);
        assert!(!r.auth_methods[0].is_terminal() && r.auth_methods[1].is_terminal());
        assert_eq!(r.auth_methods[1].args, vec!["--login"]);
        assert_eq!(r.agent_info.as_ref().unwrap().display(), Some("My Agent"));
    }

    #[test]
    fn missing_or_malformed_fields_fall_back_to_defaults() {
        let r = InitializeResult::from_value(&json!({}));
        assert_eq!(r.protocol_version, None);
        assert!(!r.agent_capabilities.can_restore());
        assert!(r.auth_methods.is_empty() && r.agent_info.is_none());
        // 类型写坏了：当没有，不让整条响应失败
        let r = InitializeResult::from_value(&json!({
            "protocolVersion": "one",
            "agentCapabilities": { "loadSession": "yes", "promptCapabilities": 3, "sessionCapabilities": { "resume": null } },
            "authMethods": "none",
            "agentInfo": { "name": 7 }
        }));
        assert_eq!(r.protocol_version, None);
        assert!(!r.agent_capabilities.load_session && !r.agent_capabilities.prompt_capabilities.image);
        assert!(!r.agent_capabilities.can_resume());
        assert!(r.auth_methods.is_empty());
        // `loadSession` 一项就够恢复
        let r = InitializeResult::from_value(&json!({ "agentCapabilities": { "loadSession": true } }));
        assert!(r.agent_capabilities.can_restore() && !r.agent_capabilities.can_resume());
    }

    #[test]
    fn implementation_prefers_the_title() {
        let i = Implementation { name: "n".into(), title: Some("T".into()), version: "1".into() };
        assert_eq!(i.display(), Some("T"));
        let i = Implementation { name: "n".into(), title: Some(" ".into()), version: "1".into() };
        assert_eq!(i.display(), Some("n"));
        assert_eq!(Implementation::default().display(), None);
    }
}
