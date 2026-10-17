//! 选择要嵌入的 Web 静态目录，并通过 `ANYCON_WEB_DIST_DIR` 交给 `rust-embed`。
//!
//! 规则（按优先级）：
//! 1. 环境变量 `ANYCON_WEB_DIST` 指向的目录（CI / Docker 可显式指定）；
//! 2. `<repo>/web/dist`，且其中有 `index.html`；
//! 3. `crates/hub/static-placeholder`（一个占位页），保证没构建过 web 时 hub 也能编译、启动。
//!
//! 注意：情况 3 下不对 `web/dist` 声明 `rerun-if-changed`（目录不存在时 cargo 会认为每次都变，
//! 导致 hub 每次都重编）。所以「先编 hub、后编 web」之后要让 hub 重新嵌入：
//! `touch crates/hub/build.rs` 或 `cargo clean -p anycon-hub`。debug 构建下 rust-embed 直接读磁盘，
//! 只要编译时选中的是 `web/dist`，之后 web 重新构建不需要重编 hub。

use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=static-placeholder");
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-env-changed=ANYCON_WEB_DIST");

    let placeholder = manifest_dir.join("static-placeholder");
    let chosen = pick(&manifest_dir).unwrap_or(placeholder);
    if !chosen.ends_with("static-placeholder") {
        println!("cargo:rerun-if-changed={}", chosen.display());
    }
    println!("cargo:rustc-env=ANYCON_WEB_DIST_DIR={}", chosen.display());
}

fn pick(manifest_dir: &Path) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ANYCON_WEB_DIST") {
        let p = PathBuf::from(dir);
        if p.join("index.html").is_file() {
            return Some(p);
        }
        println!("cargo:warning=ANYCON_WEB_DIST={} has no index.html, falling back", p.display());
    }
    let dist = manifest_dir.join("../../web/dist");
    if dist.join("index.html").is_file() {
        return dist.canonicalize().ok();
    }
    None
}
