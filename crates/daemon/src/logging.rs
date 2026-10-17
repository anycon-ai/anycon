//! 日志：`tracing`，stderr + 按天滚动的 `logs/daemon.log.<date>`；`ANYCON_LOG` / `RUST_LOG` 优先于 `log.level`，
//! `log.level` 热加载（`set_level`）。不记录任何会话内容。

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, EnvFilter, Registry};

type FilterHandle = reload::Handle<EnvFilter, Registry>;

static HANDLE: OnceLock<FilterHandle> = OnceLock::new();
static ENV_OVERRIDE: OnceLock<bool> = OnceLock::new();

/// 保持文件 appender 的后台线程活着；drop 时 flush。
pub struct LogGuard {
    _file: Option<tracing_appender::non_blocking::WorkerGuard>,
}

fn filter_for(level: &str) -> EnvFilter {
    // 只给自己的 crate 调到请求的级别，第三方保持 warn，避免 sqlx / hyper 刷屏
    let l = level.trim();
    EnvFilter::try_new(format!("warn,anycon_daemon={l},anycon_cli={l},anycon={l}"))
        .unwrap_or_else(|_| EnvFilter::new("warn,anycon_daemon=info"))
}

/// 初始化全局日志。重复调用返回 `None`（测试里多次调用无副作用）。
pub fn init(logs_dir: &Path, level: &str, retention_days: u32, to_stderr: bool) -> Option<LogGuard> {
    let env_filter =
        std::env::var("ANYCON_LOG").ok().or_else(|| std::env::var("RUST_LOG").ok()).filter(|s| !s.trim().is_empty());
    let _ = ENV_OVERRIDE.set(env_filter.is_some());
    let filter = match &env_filter {
        Some(f) => EnvFilter::try_new(f).unwrap_or_else(|_| filter_for(level)),
        None => filter_for(level),
    };
    let (filter_layer, handle) = reload::Layer::new(filter);
    let (file_layer, guard) = match std::fs::create_dir_all(logs_dir) {
        Ok(()) => {
            prune(logs_dir, retention_days);
            let appender = tracing_appender::rolling::daily(logs_dir, "daemon.log");
            let (nb, guard) = tracing_appender::non_blocking(appender);
            (Some(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(nb)), Some(guard))
        }
        Err(_) => (None, None),
    };
    let stderr_layer = to_stderr.then(|| {
        use std::io::IsTerminal;
        tracing_subscriber::fmt::layer().with_ansi(std::io::stderr().is_terminal()).with_writer(std::io::stderr)
    });
    let ok = Registry::default().with(filter_layer).with(file_layer).with(stderr_layer).try_init().is_ok();
    if !ok {
        return None;
    }
    let _ = HANDLE.set(handle);
    Some(LogGuard { _file: guard })
}

/// 热加载 `log.level`（环境变量覆盖时不动）。
pub fn set_level(level: &str) {
    if ENV_OVERRIDE.get().copied().unwrap_or(false) {
        return;
    }
    if let Some(h) = HANDLE.get() {
        let _ = h.reload(filter_for(level));
    }
}

/// 删除超过 `retention_days` 天的 `daemon.log.*`。
pub fn prune(dir: &Path, retention_days: u32) {
    if retention_days == 0 {
        return;
    }
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days as u64 * 86400);
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("daemon.log") {
            continue;
        }
        if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// 最新的日志文件。
pub fn latest_log(dir: &Path) -> Option<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.log"))
        .collect();
    files.sort_by_key(|e| e.file_name());
    files.last().map(|e| e.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_removes_only_old_log_files() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("daemon.log.2020-01-01");
        let new = d.path().join("daemon.log.2099-01-01");
        let other = d.path().join("keep.txt");
        for p in [&old, &new, &other] {
            std::fs::write(p, "x").unwrap();
        }
        let old_time = SystemTime::now() - Duration::from_secs(30 * 86400);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(old_time).unwrap();
        std::fs::File::options().write(true).open(&other).unwrap().set_modified(old_time).unwrap();
        prune(d.path(), 7);
        assert!(!old.exists());
        assert!(new.exists());
        assert!(other.exists());
        assert_eq!(latest_log(d.path()).unwrap(), new);
    }
}
