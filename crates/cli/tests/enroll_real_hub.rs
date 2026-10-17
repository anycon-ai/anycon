//! 账号内接入（ADR-36）：**`anycon` 二进制**经真实 hub（进程内）连真 daemon（进程内）。
//!
//! `anycon connect <hub-url>`（不带 `--code`）要在终端里问账号 token、接入码、口令，所以这里用伪终端（pty）当它的 stdin / stderr：
//! 测试盯着 stderr 里出现提示再敲答案（口令提示关了回显才敲）。覆盖：账号 token 登录 + 接入码、接入后 `anycon ls` / `nodes` /
//! `agents` 能用、已经是设备时不再登录就能列节点（设备 token）、错口令的退出码。逐方式的细节（退避、锁定、批准……）在
//! `src/enroll_tests.rs` 里用同一个 initiator 状态机覆盖。

mod common;
/// hub 的测试里那个最小 SMTP 服务（lettre 真的会对它发信），原样借来用，不再抄一份。
#[path = "../../hub/tests/common/smtp.rs"]
mod smtp;

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anycon_crypto::Keypair;
use anycon_daemon::testutil::{eventually, FakeWeb, TestDaemon};
use anycon_protocol::*;
use common::*;
use serde_json::json;

const PW: &str = "correct horse battery";
const T: Duration = Duration::from_secs(30);

struct TestHub {
    handle: anycon_hub::Handle,
    _dir: tempfile::TempDir,
}

impl TestHub {
    async fn start() -> TestHub {
        Self::start_with(|_| {}).await
    }

    /// 配了 SMTP（`smtp+plain://127.0.0.1:<port>`）的 hub：登录页 / CLI 多一种邮箱验证码登录。
    async fn start_with_smtp(smtp_port: u16) -> TestHub {
        Self::start_with(|c| {
            c.smtp = Some(anycon_hub::config::parse_smtp_url(&format!("smtp+plain://127.0.0.1:{smtp_port}")).unwrap());
            c.email_from = Some(anycon_hub::config::parse_email_from("Anycon <noreply@example.test>").unwrap());
        })
        .await
    }

    async fn start_with(configure: impl FnOnce(&mut anycon_hub::Config)) -> TestHub {
        let dir = tempfile::tempdir().unwrap();
        let mut c = anycon_hub::Config::new("http://127.0.0.1", dir.path()).unwrap();
        c.listen = "127.0.0.1:0".parse().unwrap();
        configure(&mut c);
        let handle = anycon_hub::run(c).await.expect("hub starts");
        TestHub { handle, _dir: dir }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.handle.addr.port())
    }

    fn admin_token(&self) -> String {
        self.handle.generated_admin_token.clone().unwrap()
    }

    async fn admin_cookie(&self) -> String {
        let res = reqwest::Client::new()
            .post(format!("{}/auth/token", self.url()))
            .header("x-anycon", "1")
            .json(&json!({ "token": self.admin_token() }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string()
    }

    /// 以管理员账号把邮箱绑上（`POST /api/me/email` → 从测试 SMTP 取绑定码 → `POST /api/me/email/verify`）。
    /// `mails_before`：这之前测试 SMTP 已经收到的信数，新的绑定码是第 `mails_before + 1` 封。
    async fn bind_email(&self, smtp: &smtp::FakeSmtp, mails_before: usize, email: &str) {
        let cookie = self.admin_cookie().await;
        let http = reqwest::Client::new();
        let post = |path: &str, body: serde_json::Value| {
            http.post(format!("{}{path}", self.url()))
                .header("x-anycon", "1")
                .header("cookie", &cookie)
                .json(&body)
                .send()
        };
        let r = post("/api/me/email", json!({ "email": email })).await.unwrap();
        assert_eq!(r.status(), 200, "{:?}", r.text().await);
        let code = smtp.wait_for(mails_before + 1).await[mails_before].code();
        let r = post("/api/me/email/verify", json!({ "email": email, "code": code })).await.unwrap();
        assert_eq!(r.status(), 200, "{:?}", r.text().await);
    }
}

/// 一对伪终端 (master, slave)。
fn openpty() -> (OwnedFd, OwnedFd) {
    let (mut m, mut s) = (0i32, 0i32);
    // SAFETY: openpty 写两个 fd。
    let rc = unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(rc, 0, "openpty");
    // SAFETY: 刚拿到的 fd，所有权交给 OwnedFd。
    unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) }
}

fn echo_on(fd: i32) -> bool {
    // SAFETY: tcgetattr 写我们的结构体。
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::tcgetattr(fd, &mut t) };
    t.c_lflag & libc::ECHO != 0
}

/// 一次 `anycon connect …` 运行：子进程的 stdin / stderr 是伪终端，stdout 另外接管道。
struct Session {
    child: tokio::process::Child,
    master: std::fs::File,
    /// 子进程写到终端上的全部内容（提示、说明、回显）。
    screen: Arc<Mutex<String>>,
    /// 留着一个 slave 的副本：看终端有没有关回显。
    slave: OwnedFd,
    /// 每段提示已经回答过几次（见 [`Session::answer`]）。
    asked: std::collections::HashMap<String, usize>,
}

impl Session {
    fn start(home: &std::path::Path, args: &[&str]) -> Session {
        let (master, slave) = openpty();
        let dup = |fd: &OwnedFd| fd.try_clone().expect("dup the pty slave");
        let mut c = base_command(home);
        c.args(args).stdin(Stdio::from(dup(&slave))).stderr(Stdio::from(dup(&slave))).stdout(Stdio::piped());
        let child = c.spawn().expect("spawn anycon");
        let master = std::fs::File::from(master);
        let screen = Arc::new(Mutex::new(String::new()));
        {
            let (mut reader, screen) = (master.try_clone().unwrap(), screen.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                // 子进程退出、slave 都关掉之后这里读到 EIO / 0，线程结束。
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    screen.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
        }
        Session { child, master, screen, slave, asked: Default::default() }
    }

    fn screen(&self) -> String {
        self.screen.lock().unwrap().clone()
    }

    /// 等终端上出现某段提示。
    async fn wait_for(&self, text: &str) {
        let t = Instant::now();
        while !self.screen().contains(text) {
            assert!(t.elapsed() < T, "never saw {text:?} on the terminal; got:\n{}", self.screen());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 回答口令类提示：等提示出现、终端关了回显，再敲（回显还开着就敲会被 TCSAFLUSH 丢掉）。
    async fn answer_secret(&mut self, prompt: &str, secret: &str) {
        self.wait_for(prompt).await;
        let t = Instant::now();
        while echo_on(self.slave.as_raw_fd()) {
            assert!(t.elapsed() < T, "echo was never turned off for {prompt:?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.master.write_all(format!("{secret}\n").as_bytes()).unwrap();
        // 密文不能出现在终端上：没有回显
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!self.screen().contains(secret), "the secret was echoed:\n{}", self.screen());
    }

    /// 回答普通提示（有回显）。同一段提示可以出现好几次（重问）：第 n 次回答等屏幕上出现第 n 个。
    async fn answer(&mut self, prompt: &str, text: &str) {
        let n = {
            let c = self.asked.entry(prompt.to_string()).or_insert(0);
            *c += 1;
            *c
        };
        let t = Instant::now();
        while self.screen().matches(prompt).count() < n {
            assert!(t.elapsed() < T, "never saw {prompt:?} (#{n}) on the terminal; got:\n{}", self.screen());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.master.write_all(format!("{text}\n").as_bytes()).unwrap();
    }

    /// 等进程结束：（退出码，stdout，终端上的全部内容）。
    async fn finish(self) -> (i32, String, String) {
        let Session { child, master, screen, slave, .. } = self;
        let out = tokio::time::timeout(T, child.wait_with_output()).await.expect("anycon finishes").unwrap();
        drop(slave);
        drop(master);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let screen = screen.lock().unwrap().clone();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), screen)
    }
}

/// 一台连着 hub、已经和一个有账号的浏览器配过对的 daemon（`anycon init` 之后的样子）。
async fn node_on_hub(hub: &TestHub) -> (TestDaemon, FakeWeb) {
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    assert!(eventually(T, || async { td.ctx.hub_state.get().connected }).await);
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("setup");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Setup Browser", Keypair::generate()).await;
    web.pair(&begin.code).await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    (td, web)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_connect_command_joins_with_a_join_code_then_the_device_token_lists_nodes_without_signing_in() {
    let hub = TestHub::start().await;
    let (td, web) = node_on_hub(&hub).await;
    let mut owner = td.client("owner");
    owner.call::<serde_json::Value>(&td, methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": PW })).await.unwrap();
    let made: NodeEnrollCodeResult = owner.call(&td, methods::NODE_ENROLL_CODE, json!({})).await.unwrap();

    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("cli-home");
    let url = hub.url();

    // ---- 1. 全新的 CLI：账号 token 登录（不回显）、选节点、敲接入码
    let mut s = Session::start(&home, &["--node", "test-node", "connect", &url, "--via", "code", "--name", "cli-bin"]);
    s.answer_secret("Account token: ", &hub.admin_token()).await;
    s.answer("Join code (6 digits): ", &made.code).await;
    let (code, stdout, screen) = s.finish().await;
    assert_eq!(code, 0, "stdout: {stdout}\nterminal:\n{screen}");
    assert!(stdout.contains("Joined node test-node"), "{stdout}");
    assert!(!screen.contains(&hub.admin_token()), "the account token was echoed:\n{screen}");
    // 账号 token / 会话 cookie 不在磁盘上
    let client_dir = std::fs::read_dir(home.join("client")).unwrap().next().unwrap().unwrap().path();
    for f in std::fs::read_dir(&client_dir).unwrap().flatten() {
        let text = String::from_utf8_lossy(&std::fs::read(f.path()).unwrap()).into_owned();
        assert!(!text.contains(&hub.admin_token()), "{:?}", f.path());
    }

    // ---- 2. 接入后就是普通的已配对设备：ls / nodes / agents（Noise 加密）都能用
    let ls = anycon(&home, &["ls", "--json"], None, &[]).await;
    assert_eq!(ls.code, 0, "{}", ls.stderr);
    assert_eq!(ls.json()["sessions"].as_array().unwrap().len(), 0);
    let nodes = anycon(&home, &["nodes", "--json"], None, &[]).await.json();
    assert_eq!(
        (nodes["nodes"][0]["name"].as_str(), nodes["nodes"][0]["status"].as_str()),
        (Some("test-node"), Some("online"))
    );
    let agents = anycon(&home, &["--node", "test-node", "agents", "--json"], None, &[]).await;
    assert_eq!(agents.code, 0, "{}", agents.stderr);
    // daemon 侧认的是握手里自报的名字和类型
    let devices: NodeDevicesResult = owner.call(&td, methods::NODE_DEVICES, json!({})).await.unwrap();
    let cli = devices.devices.iter().find(|d| d.name == "cli-bin").expect("the CLI is a device of the node");
    assert_eq!(cli.kind, DeviceKind::Cli);

    // ---- 3. 已经是这个 hub 上的设备：不给 --node，用节点口令再接入一次；列账号里的节点用设备 token，整个过程不问账号 token
    let mut s = Session::start(&home, &["connect", &url, "--via", "password", "--name", "cli-bin"]);
    s.answer_secret("Node password: ", PW).await;
    let (code, stdout, screen) = s.finish().await;
    assert_eq!(code, 0, "stdout: {stdout}\nterminal:\n{screen}");
    assert!(!screen.contains("Account token"), "signed in although the CLI was already a device:\n{screen}");
    assert!(stdout.contains("Joined node test-node"), "{stdout}");

    // ---- 4. 错口令：退出码 5，什么都不变，失败记了一次
    let mut s = Session::start(&home, &["connect", &url, "--via", "password"]);
    s.answer_secret("Node password: ", "definitely not it").await;
    let (code, stdout, screen) = s.finish().await;
    assert_eq!(code, 5, "stdout: {stdout}\nterminal:\n{screen}");
    assert!(screen.contains("wrong"), "{screen}");
    let st: NodeEnrollStatusResult = owner.call(&td, methods::NODE_ENROLL_STATUS, json!({})).await.unwrap();
    assert_eq!(st.password.failures, 1);
    assert_eq!(anycon(&home, &["agents", "--json"], None, &[]).await.code, 0, "the earlier enrollment still works");
    drop(web);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_connect_command_needs_a_terminal_and_a_signed_in_account() {
    let hub = TestHub::start().await;
    let (_td, web) = node_on_hub(&hub).await;
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("cli-home");
    let url = hub.url();
    // 不是终端（管道）：参数错误 2，提示脚本里用 --code
    let r = anycon(&home, &["connect", &url, "--via", "password"], Some("x\n"), &[]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("--code"), "{}", r.stderr);
    // 账号 token 不对：5
    let mut s = Session::start(&home, &["--node", "test-node", "connect", &url, "--via", "password"]);
    s.answer_secret("Account token: ", "not-the-admin-token").await;
    let (code, _, screen) = s.finish().await;
    assert_eq!(code, 5, "terminal:\n{screen}");
    drop(web);
}

/// 一个不等于 `real` 的 6 位数字码（测试里「输错」用）。
fn wrong_code(real: &str) -> &'static str {
    if real == "000000" {
        "111111"
    } else {
        "000000"
    }
}

/// 这个 CLI home 下有没有落盘过名叫 `test-node` 的节点。
fn home_has_node(home: &std::path::Path) -> bool {
    let Ok(dirs) = std::fs::read_dir(home.join("client")) else { return false };
    dirs.flatten().any(|d| std::fs::read_to_string(d.path().join("nodes.json")).is_ok_and(|t| t.contains("test-node")))
}

/// 邮箱验证码登录（ADR-37，spec/cli.md § 3.3）：`anycon` 二进制 ↔ 真 hub（配了 SMTP）↔ 测试里起的 SMTP 服务。
/// 绑定邮箱的账号能用邮箱 + 码登录并接入节点；码输错会重问；没绑定的邮箱得到一模一样的回答（但没有信）；同一个邮箱 60 秒内
/// 再要一次码是 429，CLI 写明要等多久。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_connect_command_signs_in_with_an_email_code_sent_by_the_real_hub_over_smtp() {
    let smtp = smtp::FakeSmtp::start(false).await;
    let hub = TestHub::start_with_smtp(smtp.port()).await;
    let (td, web) = node_on_hub(&hub).await;
    let mut owner = td.client("owner");
    owner.call::<serde_json::Value>(&td, methods::NODE_SET_ENROLL_PASSWORD, json!({ "password": PW })).await.unwrap();
    hub.bind_email(&smtp, 0, "Owner@Example.Test").await;
    assert_eq!(smtp.mails().len(), 1, "the binding code is the only mail so far");

    let d = tempfile::tempdir().unwrap();
    let url = hub.url();

    // ---- 1. 全新的 CLI，hub 同时开了 token 和邮箱：先问用哪种（默认邮箱）、问邮箱地址（大小写不拘）、问码
    let home = d.path().join("cli-home");
    let mut s =
        Session::start(&home, &["--node", "test-node", "connect", &url, "--via", "password", "--name", "cli-mail"]);
    s.answer("[email/token] (email): ", "").await;
    s.answer("Email address: ", "  Owner@Example.Test ").await;
    s.wait_for("Code from the email: ").await;
    // 提示不能说「已发送到你的邮箱」：hub 对绑定和没绑定的邮箱回答一样
    assert!(s.screen().contains("If that email is bound to an account"), "{}", s.screen());
    let mails = smtp.wait_for(2).await;
    assert_eq!(mails[1].rcpt_to, vec!["owner@example.test"], "the hub normalizes the address");
    let real = mails[1].code();
    // 输错一次：重问，不重新发信
    s.answer("Code from the email: ", wrong_code(&real)).await;
    s.wait_for("wrong or has expired").await;
    s.answer("Code from the email: ", &real).await;
    s.answer_secret("Node password: ", PW).await;
    let (code, stdout, screen) = s.finish().await;
    assert_eq!(code, 0, "stdout: {stdout}\nterminal:\n{screen}");
    assert!(stdout.contains("Joined node test-node"), "{stdout}");
    assert_eq!(smtp.mails().len(), 2, "re-asking for the code does not send another mail");
    // 登录 cookie、验证码都不在磁盘上
    let client_dir = std::fs::read_dir(home.join("client")).unwrap().next().unwrap().unwrap().path();
    for f in std::fs::read_dir(&client_dir).unwrap().flatten() {
        let text = String::from_utf8_lossy(&std::fs::read(f.path()).unwrap()).into_owned();
        assert!(!text.contains(&real) && !text.contains("anycon_session"), "{:?}", f.path());
    }
    // 接入完成：普通的已配对设备
    let agents = anycon(&home, &["--node", "test-node", "agents", "--json"], None, &[]).await;
    assert_eq!(agents.code, 0, "{}", agents.stderr);

    // ---- 2. 没绑定的邮箱：同样的提示、同样要输码，但没有信；连错三次 → 退出码 5，说要重新来
    let home2 = d.path().join("cli-home-2");
    let mut s = Session::start(
        &home2,
        &["--node", "test-node", "connect", &url, "--via", "password", "--email", "ghost@example.test"],
    );
    s.wait_for("Code from the email: ").await;
    assert!(s.screen().contains("If that email is bound to an account"), "{}", s.screen());
    s.answer("Code from the email: ", "123456").await;
    s.answer("Code from the email: ", "234567").await;
    s.answer("Code from the email: ", "345678").await;
    let (code, stdout, screen) = s.finish().await;
    assert_eq!(code, 5, "stdout: {stdout}\nterminal:\n{screen}");
    assert!(screen.contains("run the command again"), "{screen}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(smtp.mails().len(), 2, "nothing is sent to an address that is not bound");
    assert!(!home_has_node(&home2));

    // ---- 3. 同一个邮箱 60 秒内再要一次码：hub 回 429（带 Retry-After），CLI 退出码 1，写明要等多少秒；不问码
    let home3 = d.path().join("cli-home-3");
    let s = Session::start(
        &home3,
        &["--node", "test-node", "connect", &url, "--via", "password", "--email", "owner@example.test"],
    );
    let (code, stdout, screen) = s.finish().await;
    assert_eq!(code, 1, "stdout: {stdout}\nterminal:\n{screen}");
    assert!(screen.contains("too many requests") && screen.contains("seconds and try again"), "{screen}");
    assert!(!screen.contains("Code from the email"), "{screen}");
    assert!(!home_has_node(&home3));

    // ---- 4. hub 的审计：邮箱登录成功 / 失败各有记录，方式是 email；不含邮箱和码
    let admin = hub.admin_cookie().await;
    let audit: serde_json::Value = reqwest::Client::new()
        .get(format!("{url}/api/admin/audit"))
        .header("cookie", &admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let events = audit.as_array().unwrap();
    let count = |event: &str| events.iter().filter(|e| e["event"] == event && e["detail"]["method"] == "email").count();
    assert_eq!(count("login_ok"), 1, "{audit}");
    assert_eq!(count("login_fail"), 4, "one wrong code in step 1, three for the unbound address: {audit}");
    let dump = audit.to_string().to_ascii_lowercase();
    assert!(!dump.contains("example.test") && !dump.contains(&real), "{dump}");
    drop(web);
}
