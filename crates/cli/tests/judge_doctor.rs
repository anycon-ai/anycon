//! doctor 的判断后端项：未配置是 ok，失效的位置显示 warn，不输出后端秘密。

mod common;

use anycon_daemon::testutil::TestDaemon;
use common::anycon;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn doctor_prints_ok_for_no_configuration_and_warn_for_a_missing_backend() {
    let td = TestDaemon::start_with(&[], true).await;
    let result = anycon(&td.paths.home, &["doctor"], None, &[]).await;
    let line = result.stdout.lines().find(|l| l.split_whitespace().nth(1) == Some("judge")).unwrap();
    assert_eq!(line.split_whitespace().next(), Some("ok"));
    assert!(line.contains("not configured") && line.contains("today: 0 / 20 calls"));
    anycon_daemon::config::set_value(&td.paths, "judge.extract", "missing").unwrap();
    td.ctx.config.reload().unwrap();
    let result = anycon(&td.paths.home, &["doctor"], None, &[]).await;
    let line = result.stdout.lines().find(|l| l.split_whitespace().nth(1) == Some("judge")).unwrap();
    assert_eq!(line.split_whitespace().next(), Some("warn"));
    assert!(line.contains("extract: missing (backend does not exist)"));
    td.shutdown().await;
}
