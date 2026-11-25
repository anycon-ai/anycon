//! 判断后端的配置解析与校验。反序列化错误只描述字段要求，不回显配置值。

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::judge::{AgentBackendConfig, ApiBackendConfig, ApiFormat, ApiKeySource, Effort};
use crate::util::parse_duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JudgeConfig {
    pub extract: String,
    pub triage: String,
    pub advisor: String,
    pub daily_calls: u32,
    pub backends: BTreeMap<String, JudgeBackendConfig>,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            extract: String::new(),
            triage: String::new(),
            advisor: String::new(),
            daily_calls: 20,
            backends: BTreeMap::new(),
        }
    }
}

/// 已补齐缺省值的配置；密钥的 Debug 由 ApiKeySource 脱敏。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(into = "RawBackend")]
pub enum JudgeBackendConfig {
    Api(ApiBackendConfig),
    Agent(AgentBackendConfig),
}

#[derive(Clone, Serialize, Deserialize)]
struct RawBackend {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<ApiFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<Effort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
}

impl<'de> Deserialize<'de> for JudgeBackendConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        let raw: RawBackend = serde_json::from_value(value).map_err(|_| {
            serde::de::Error::custom("invalid judge backend fields (kind is required; check field types and format)")
        })?;
        raw.resolve().map_err(serde::de::Error::custom)
    }
}

fn required(value: Option<String>, field: &str) -> Result<String, String> {
    value.filter(|s| !s.trim().is_empty()).ok_or_else(|| format!("{field} is required"))
}

/// 两种后端的 `timeout` 上限都是 60 分钟。一个节点同时只有一次判断调用（§ 25.2），调用越长，别的位置等得越久。
pub const MAX_TIMEOUT_SECS: u64 = 60 * 60;

fn timeout(value: Option<String>, default: u64, max: u64) -> Result<Duration, String> {
    let d = match value {
        Some(s) => parse_duration(&s).map_err(|_| "timeout must be a duration".to_string())?,
        None => Duration::from_secs(default),
    };
    if !(Duration::from_secs(10)..=Duration::from_secs(max)).contains(&d) {
        return Err(format!("timeout must be between 10s and {max}s"));
    }
    Ok(d)
}

/// 只认规格列出的三个回环主机，不做 DNS 解析。
pub fn is_loopback_http(base_url: &str) -> bool {
    base_url.parse::<http::Uri>().is_ok_and(|uri| {
        uri.scheme_str() == Some("http")
            && uri
                .host()
                .is_some_and(|h| matches!(h.to_ascii_lowercase().as_str(), "127.0.0.1" | "[::1]" | "localhost"))
            && uri.authority().is_some_and(|a| {
                !a.as_str().contains('@') && (Some(a.as_str()) == uri.host() || uri.port_u16().is_some())
            })
    })
}

fn base_url(value: Option<String>, format: ApiFormat) -> Result<String, String> {
    let s = value.unwrap_or_else(|| format.default_base_url().into());
    let valid = s.parse::<http::Uri>().is_ok_and(|uri| {
        uri.host().is_some_and(|h| !h.is_empty())
            && uri.authority().is_some_and(|a| {
                !a.as_str().contains('@') && (Some(a.as_str()) == uri.host() || uri.port_u16().is_some())
            })
            && uri.query().is_none()
            && !s.contains('#')
            && (uri.scheme_str() == Some("https") || is_loopback_http(&s))
    });
    if !valid {
        return Err("base_url must use https or loopback http, without credentials, query or fragment".into());
    }
    Ok(s.trim_end_matches('/').into())
}

impl RawBackend {
    fn resolve(self) -> Result<JudgeBackendConfig, String> {
        if self.api_key.is_some() && self.api_key_env.is_some() {
            return Err("api_key and api_key_env cannot both be set".into());
        }
        match self.kind.as_str() {
            "api" => {
                let format = self.format.ok_or("format is required (anthropic or openai)")?;
                if self.effort.is_some() && format != ApiFormat::Anthropic {
                    return Err("effort only applies to format = anthropic".into());
                }
                let model = required(self.model, "model")?;
                let max_output_tokens = self.max_output_tokens.unwrap_or(2048);
                if !(1..=16384).contains(&max_output_tokens) {
                    return Err("max_output_tokens must be between 1 and 16384".into());
                }
                let api_key = match (self.api_key_env, self.api_key) {
                    (Some(name), _) => ApiKeySource::Env(name),
                    (_, Some(value)) => ApiKeySource::Literal(value),
                    _ => ApiKeySource::None,
                };
                Ok(JudgeBackendConfig::Api(ApiBackendConfig {
                    format,
                    model,
                    base_url: base_url(self.base_url, format)?,
                    api_key,
                    max_output_tokens,
                    effort: self.effort,
                    timeout: timeout(self.timeout, 120, MAX_TIMEOUT_SECS)?,
                }))
            }
            "agent" => {
                if self.effort.is_some() {
                    return Err("effort only applies to kind = api with format = anthropic".into());
                }
                Ok(JudgeBackendConfig::Agent(AgentBackendConfig {
                    agent: required(self.agent, "agent")?,
                    model: self.model,
                    timeout: timeout(self.timeout, 300, MAX_TIMEOUT_SECS)?,
                }))
            }
            _ => Err("kind must be api or agent".into()),
        }
    }
}

impl From<JudgeBackendConfig> for RawBackend {
    fn from(cfg: JudgeBackendConfig) -> Self {
        let mut raw = Self {
            kind: String::new(),
            format: None,
            base_url: None,
            model: None,
            api_key_env: None,
            api_key: None,
            max_output_tokens: None,
            effort: None,
            timeout: None,
            agent: None,
        };
        match cfg {
            JudgeBackendConfig::Api(c) => {
                raw.kind = "api".into();
                raw.format = Some(c.format);
                raw.base_url = Some(c.base_url);
                raw.model = Some(c.model);
                raw.max_output_tokens = Some(c.max_output_tokens);
                raw.effort = c.effort;
                raw.timeout = Some(format!("{}ms", c.timeout.as_millis()));
                match c.api_key {
                    ApiKeySource::Env(name) => raw.api_key_env = Some(name),
                    ApiKeySource::Literal(value) => raw.api_key = Some(value),
                    ApiKeySource::None => {}
                }
            }
            JudgeBackendConfig::Agent(c) => {
                raw.kind = "agent".into();
                raw.agent = Some(c.agent);
                raw.model = c.model;
                raw.timeout = Some(format!("{}ms", c.timeout.as_millis()));
            }
        }
        raw
    }
}

impl JudgeConfig {
    pub fn validate(&self) -> Result<(), String> {
        for (name, cfg) in &self.backends {
            if name.is_empty()
                || name.len() > 40
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err("backend names must be 1-40 characters from [A-Za-z0-9_-]".into());
            }
            RawBackend::from(cfg.clone()).resolve()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ConfigStore, Paths};

    fn load(text: &str) -> Result<Config, crate::config::ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::write(paths.config_toml(), text).unwrap();
        Config::load(&paths)
    }

    #[test]
    fn defaults_and_backend_defaults_roundtrip() {
        let cfg = load("").unwrap();
        assert_eq!(cfg.judge, JudgeConfig::default());
        assert!(!cfg.rules.extract_auto);
        let cfg = load(
            "[judge]\n\
             extract='absent'\n\
             [judge.backends.a]\n\
             kind='api'\n\
             format='anthropic'\n\
             model='m'\n\
             [judge.backends.b]\n\
             kind='api'\n\
             format='openai'\n\
             model='m'\n\
             [judge.backends.c]\n\
             kind='agent'\n\
             agent='fake'",
        )
        .unwrap();
        assert_eq!(cfg.judge.extract, "absent");
        for (name, url) in [("a", "https://api.anthropic.com"), ("b", "https://api.openai.com/v1")] {
            let JudgeBackendConfig::Api(c) = &cfg.judge.backends[name] else { panic!() };
            assert_eq!(c.base_url, url);
            assert_eq!(c.max_output_tokens, 2048);
            assert_eq!(c.effort, None);
            assert_eq!(c.timeout, Duration::from_secs(120));
            assert_eq!(c.api_key, ApiKeySource::None);
        }
        let JudgeBackendConfig::Agent(c) = &cfg.judge.backends["c"] else { panic!() };
        assert_eq!(c.timeout, Duration::from_secs(300));
        assert!(c.model.is_none());
        assert_eq!(serde_json::from_value::<Config>(serde_json::to_value(&cfg).unwrap()).unwrap(), cfg);
    }

    #[test]
    fn names_required_fields_types_and_bounds_are_validated_without_secrets() {
        for name in ["", "bad.name", "bad name", "中文", &"x".repeat(41)] {
            assert!(load(&format!("[judge.backends.'{name}']\nkind='agent'\nagent='fake'")).is_err());
        }
        for name in ["a", "A_z-9", &"x".repeat(40)] {
            assert!(load(&format!("[judge.backends.'{name}']\nkind='agent'\nagent='fake'")).is_ok());
        }
        for body in [
            "model='x'",
            "kind='other'",
            "kind='api'",
            "kind='api'\nformat='openai'",
            "kind='api'\nformat='other'\nmodel='m'",
            "kind='api'\nformat='openai'\nmodel=''",
            "kind='agent'",
            "kind='agent'\nagent=''",
            "kind='agent'\nagent=5",
            "kind='agent'\nagent='a'\ntimeout='9s'",
            "kind='agent'\nagent='a'\ntimeout='3601s'",
            "kind='agent'\nagent='a'\neffort='high'",
            "kind='agent'\nagent='a'\ntimeout='invalid'",
        ] {
            assert!(load(&format!("[judge.backends.a]\n{body}\napi_key='PRIVATE-KEY'")).is_err(), "{body}");
        }
        let api = "[judge.backends.a]\nkind='api'\nformat='openai'\nmodel='m'\napi_key='PRIVATE-KEY'\n";
        for extra in [
            "max_output_tokens=0",
            "max_output_tokens=16385",
            "max_output_tokens=-1",
            "max_output_tokens='x'",
            "timeout='9s'",
            "timeout='3601s'",
            "effort='high'",
            "timeout='broken'",
            "timeout=120",
            "api_key_env='K'",
            "model=4",
        ] {
            let error = load(&format!("{api}{extra}")).unwrap_err();
            assert!(!format!("{error:?} {error}").contains("PRIVATE-KEY"));
        }
        for extra in ["max_output_tokens=1\ntimeout='10s'", "max_output_tokens=16384\ntimeout='60m'"] {
            let cfg = load(&format!("{api}{extra}")).unwrap();
            assert!(!format!("{cfg:?}").contains("PRIVATE-KEY"));
        }
        let anthropic = "[judge.backends.a]\nkind='api'\nformat='anthropic'\nmodel='m'\n";
        for (value, expected) in [
            ("low", Effort::Low),
            ("medium", Effort::Medium),
            ("high", Effort::High),
            ("xhigh", Effort::Xhigh),
            ("max", Effort::Max),
        ] {
            let cfg = load(&format!("{anthropic}effort='{value}'")).unwrap();
            let JudgeBackendConfig::Api(c) = &cfg.judge.backends["a"] else { panic!() };
            assert_eq!(c.effort, Some(expected));
            assert_eq!(serde_json::from_value::<Config>(serde_json::to_value(&cfg).unwrap()).unwrap(), cfg);
        }
        for value in ["'High'", "'none'", "''", "3"] {
            assert!(load(&format!("{anthropic}effort={value}")).is_err(), "{value}");
        }
        for timeout in ["10s", "60m"] {
            assert!(load(&format!("[judge.backends.a]\nkind='agent'\nagent='a'\ntimeout='{timeout}'")).is_ok());
        }
        for n in ["-1", "4294967296", "1.5", "'20'"] {
            assert!(load(&format!("[judge]\ndaily_calls={n}")).is_err());
        }
        for n in [0, 1000, 1001, u32::MAX] {
            assert_eq!(load(&format!("[judge]\ndaily_calls={n}")).unwrap().judge.daily_calls, n);
        }
        assert!(load("[rules]\nextract_auto='yes'").is_err());
    }

    #[test]
    fn urls_only_allow_https_or_exact_loopback_http_and_trim_slashes() {
        for url in [
            "http://127.0.0.1:8080/",
            "http://[::1]:8080/v1///",
            "http://localhost/v1",
            "https://example.com/v1/",
            "https://localhost",
        ] {
            let cfg =
                load(&format!("[judge.backends.a]\nkind='api'\nformat='openai'\nmodel='m'\nbase_url='{url}'")).unwrap();
            let JudgeBackendConfig::Api(c) = &cfg.judge.backends["a"] else { panic!() };
            assert_eq!(c.base_url, url.trim_end_matches('/'));
        }
        for url in [
            "http://example.com",
            "http://localhost.evil",
            "http://127.0.0.2",
            "http://127.1",
            "http://[::ffff:127.0.0.1]",
            "http://0.0.0.0",
            "http://localhost:abc",
            "https://example.com:65536",
            "https://example.com:",
            "http://192.168.0.1",
            "ftp://localhost",
            "http:///",
            "https://",
            "localhost",
            "http://user@localhost",
            "https://example.com?q=1",
            "https://example.com/#x",
        ] {
            assert!(
                load(&format!("[judge.backends.a]\nkind='api'\nformat='openai'\nmodel='m'\nbase_url='{url}'")).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn whole_section_and_auto_switch_reload() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::write(paths.config_toml(), "").unwrap();
        let store = ConfigStore::load(&paths).unwrap();
        std::fs::write(
            paths.config_toml(),
            "[rules]\n\
             extract_auto=true\n\
             [judge]\n\
             extract='a'\n\
             triage='a'\n\
             daily_calls=12\n\
             [judge.backends.a]\n\
             kind='agent'\n\
             agent='fake'\n\
             model='m'\n\
             timeout='11s'",
        )
        .unwrap();
        assert!(store.reload().unwrap());
        assert!(store.get().rules.extract_auto);
        assert_eq!(store.get().judge.daily_calls, 12);
        assert_eq!(store.get().judge.backends.len(), 1);
        std::fs::write(paths.config_toml(), "[judge.backends.a]\nkind='no'").unwrap();
        assert!(store.reload().is_err());
        assert_eq!(store.get().judge.daily_calls, 12);
        for key in
            ["judge.extract", "judge.triage", "judge.daily_calls", "judge.backends.a.api_key", "rules.extract_auto"]
        {
            assert!(crate::config::is_hot_key(key));
        }
    }
}

#[cfg(test)]
mod twin_config_tests {
    use crate::config::{is_hot_key, ConfigStore, Paths};

    #[test]
    fn twin_and_advisor_defaults_and_hot_reload() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::write(paths.config_toml(), "").unwrap();
        let store = ConfigStore::load(&paths).unwrap();
        assert!(store.get().twin.enabled);
        assert!(store.get().judge.advisor.is_empty());
        assert!(is_hot_key("twin.enabled") && is_hot_key("judge.advisor"));
        std::fs::write(paths.config_toml(), "[twin]\nenabled=false\n[judge]\nadvisor='expert'\n").unwrap();
        store.reload().unwrap();
        assert!(!store.get().twin.enabled);
        assert_eq!(store.get().judge.advisor, "expert");
    }
}
