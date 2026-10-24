//! 日志纪律（docs/spec/hub.md § 10）在 OAuth 登录上：跑完一遍成功、失败、超时、关联、解绑的流程，把 hub 的 trace 级日志
//! 收集起来，断言里面没有授权码、`state`、PKCE verifier、access token、client secret、提供方的用户 id 和邮箱；
//! 授权码、access token、`state` 原文、client secret 也不能落在数据目录的任何文件里（库里只有 `state` 的哈希；
//! PKCE verifier 在 state 存活期间会在库里，所以不查它）。

mod common;
#[path = "common/fake_oauth.rs"]
mod fake_oauth;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_hub::config::OAuthProvider;
use common::*;
use fake_oauth::{FakeOAuth, FakeUser};
use serde_json::Value;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture() -> Capture {
    let c = Capture::default();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("anycon_hub=trace"))
        .with_writer(c.clone())
        .with_ansi(false)
        .init();
    c
}

fn query_of(location: &str, key: &str) -> Option<String> {
    let url = url::Url::parse("https://hub.test").unwrap().join(location).unwrap();
    url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

/// 数据目录里所有文件的内容拼在一起（SQLite 主库、WAL……）。
fn data_dir_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut all = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            all.extend(data_dir_bytes(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            all.extend(bytes);
        }
    }
    all
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[tokio::test]
async fn oauth_flows_never_log_codes_states_tokens_secrets_subjects_or_emails() {
    let capture = capture();
    let fake = FakeOAuth::start().await;
    let (hub, _mailer) = TestHub::start_multi_with(|c| {
        fake.configure(c, &[OAuthProvider::Github, OAuthProvider::Google]);
        c.timing.oauth_timeout = Duration::from_millis(300);
    })
    .await;
    let http = &hub.http;
    let host = "hub.test";

    // 提供方那边的用户：独特的 id / 邮箱 / 名字，方便在日志里搜
    let github_subject = "918273645501";
    let google_subject = "google-sub-4c1f9a7e2b";
    let github_email = "gh-secret-user@leak-check.example";
    let google_email = "g-secret-user@leak-check.example";
    let github_user = || FakeUser::new(github_subject).name("Gh Secret Name").email(github_email, true);
    let google_user = || FakeUser::new(google_subject).name("G Secret Name").email(google_email, true);

    let mut flow_secrets: Vec<(String, String)> = Vec::new();
    let run = |provider: &'static str, user: FakeUser, query: &'static str, cookie: Option<String>| {
        fake.set_user(provider, user);
        let (hub, fake, host) = (&hub, &fake, host);
        async move {
            let start = hub
                .http
                .get(hub.url(&format!("/auth/oauth/{provider}/start{query}")))
                .header("host", host)
                .header("cookie", cookie.clone().unwrap_or_default())
                .send()
                .await
                .unwrap();
            assert_eq!(start.status(), 302);
            let authorize_url = start.headers()["location"].to_str().unwrap().to_string();
            let state = query_of(&authorize_url, "state").unwrap();
            let oauth_cookie = start.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
            let back = fake.authorize(&authorize_url).await;
            let back = url::Url::parse(back.headers()["location"].to_str().unwrap()).unwrap();
            let code = back.query_pairs().find(|(k, _)| k == "code").map(|(_, v)| v.into_owned());
            let cookies = match cookie {
                Some(c) => format!("{oauth_cookie}; {c}"),
                None => oauth_cookie,
            };
            let done = hub
                .http
                .get(hub.url(&format!("{}?{}", back.path(), back.query().unwrap())))
                .header("host", host)
                .header("cookie", cookies)
                .send()
                .await
                .unwrap();
            assert_eq!(done.status(), 302);
            let location = done.headers()["location"].to_str().unwrap().to_string();
            let session = done
                .headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .find(|c| c.starts_with("anycon_session="))
                .map(|c| c.split(';').next().unwrap().to_string());
            (state, code, location, session)
        }
    };

    // 1. 成功的注册 / 登录（两个提供方）、关联、解绑
    let (state_a, code_a, loc, github_session) = run("github", github_user(), "?next=%2Fchat", None).await;
    assert_eq!(loc, "/chat");
    let github_session = github_session.expect("signed in");
    flow_secrets.push(("github state".into(), state_a));
    flow_secrets.push(("github code".into(), code_a.unwrap()));
    let (state_b, code_b, _, _) = run("google", google_user(), "?link=1", Some(github_session.clone())).await;
    flow_secrets.push(("google state (link)".into(), state_b));
    flow_secrets.push(("google code (link)".into(), code_b.unwrap()));
    assert_eq!(
        hub.send(reqwest::Method::DELETE, "/api/me/identities/github", &github_session, Value::Null).await.status(),
        204
    );
    let (state_c, code_c, loc, _) = run("google", google_user(), "", None).await;
    assert_eq!(loc, "/");
    flow_secrets.push(("google state".into(), state_c));
    flow_secrets.push(("google code".into(), code_c.unwrap()));

    // 2. 失败：用户拒绝、换 token 失败、取用户信息失败、超时、state 不对
    fake.set_behavior(|b| b.deny_next_authorize = true);
    let (state_d, _, loc, _) = run("github", github_user(), "", None).await;
    assert!(loc.contains("oauth_error=cancelled"), "{loc}");
    flow_secrets.push(("denied state".into(), state_d));
    fake.set_behavior(|b| b.token_status = Some(500));
    let (state_e, code_e, loc, _) = run("github", github_user(), "", None).await;
    assert!(loc.contains("oauth_error=provider_error"), "{loc}");
    flow_secrets.push(("token failure state".into(), state_e));
    flow_secrets.push(("token failure code".into(), code_e.unwrap()));
    fake.set_behavior(|b| *b = Default::default());
    fake.set_behavior(|b| b.userinfo_status = Some(502));
    let (state_f, code_f, loc, _) = run("google", google_user(), "", None).await;
    assert!(loc.contains("oauth_error=provider_error"), "{loc}");
    flow_secrets.push(("userinfo failure state".into(), state_f));
    flow_secrets.push(("userinfo failure code".into(), code_f.unwrap()));
    fake.set_behavior(|b| *b = Default::default());
    fake.set_behavior(|b| b.token_delay = Duration::from_secs(3));
    let (state_g, code_g, loc, _) = run("github", github_user(), "", None).await;
    assert!(loc.contains("oauth_error=provider_error"), "{loc}");
    flow_secrets.push(("timeout state".into(), state_g));
    flow_secrets.push(("timeout code".into(), code_g.unwrap()));
    fake.set_behavior(|b| *b = Default::default());
    // 伪造的回调：state 和 cookie 一致但库里没有；授权码是攻击者随便填的
    let forged_state = "d".repeat(64);
    let forged_code = "attacker-supplied-code-0123456789";
    let res = http
        .get(hub.url(&format!("/auth/callback/github?code={forged_code}&state={forged_state}")))
        .header("cookie", format!("anycon_oauth={forged_state}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 302);
    flow_secrets.push(("forged state".into(), forged_state));
    flow_secrets.push(("forged code".into(), forged_code.into()));

    // 3. 收集提供方那边记到的 access token、PKCE verifier
    for r in fake.requests_to("token") {
        flow_secrets.push(("PKCE verifier".into(), r.params["code_verifier"].clone()));
    }
    for r in fake
        .requests_to("user")
        .iter()
        .chain(fake.requests_to("userinfo").iter())
        .chain(fake.requests_to("emails").iter())
    {
        let token = r.headers["authorization"].strip_prefix("Bearer ").unwrap().to_string();
        flow_secrets.push(("access token".into(), token));
    }
    assert!(flow_secrets.len() > 14, "collected secrets from all the flows: {}", flow_secrets.len());

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("oauth sign-in"), "the flows were logged at all:\n{logs}");
    let mut must_not_appear = flow_secrets.clone();
    must_not_appear.push(("client secret".into(), fake_oauth::CLIENT_SECRET.into()));
    must_not_appear.push(("github subject".into(), github_subject.into()));
    must_not_appear.push(("google subject".into(), google_subject.into()));
    for email in [github_email, google_email] {
        must_not_appear.push(("email address".into(), email.into()));
        must_not_appear.push(("email local part".into(), email.split('@').next().unwrap().into()));
    }
    for name in ["Gh Secret Name", "G Secret Name"] {
        must_not_appear.push(("display name".into(), name.into()));
    }
    must_not_appear.push(("session cookie".into(), github_session.split_once('=').unwrap().1.into()));
    for (what, secret) in &must_not_appear {
        assert!(secret.len() >= 8, "{what}: a meaningful needle ({secret:?})");
        assert!(!logs.contains(secret.as_str()), "{what} leaked into the logs:\n{logs}");
    }

    // 数据目录：授权码、access token、state 原文、client secret 不落盘（verifier 与 state 行同生共死，不查）
    let files = data_dir_bytes(hub.dir.path());
    assert!(contains(&files, github_subject), "sanity: the identity (provider, subject) is stored, by design");
    for (what, secret) in flow_secrets.iter().filter(|(what, _)| *what != "PKCE verifier") {
        assert!(!contains(&files, secret), "{what} was written to the data directory");
    }
    assert!(!contains(&files, fake_oauth::CLIENT_SECRET), "the client secret is never stored");
}
