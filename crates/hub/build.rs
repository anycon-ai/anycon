//! 选择要嵌入的两份静态目录，并分别通过 `ANYCON_WEB_DIST_DIR` / `ANYCON_WEB_PREVIEW_DIST_DIR` 交给 `rust-embed`：
//! Web UI（`web/dist`）和预览运行时（`web/dist-preview`）。两份分开构建、分开嵌入、分开提供
//! （docs/spec/hub.md § 13.2）：预览运行时的文件不能出现在主域名上。
//!
//! 每一份的规则（按优先级）：
//! 1. 环境变量（Web UI 是 `ANYCON_WEB_DIST`，预览运行时是 `ANYCON_WEB_PREVIEW_DIST`）指向的目录（CI / Docker 可显式指定）；
//! 2. `<repo>/web/dist`（预览运行时是 `<repo>/web/dist-preview`），且其中有 `index.html`；
//! 3. `crates/hub/static-placeholder`（预览运行时是 `static-placeholder-preview`）里的占位页，
//!    保证没构建过 web 时 hub 也能编译、启动。
//!
//! 注意：情况 3 下不对 `web/dist` / `web/dist-preview` 声明 `rerun-if-changed`（目录不存在时 cargo 会认为每次都变，
//! 导致 hub 每次都重编）。所以「先编 hub、后编 web」之后要让 hub 重新嵌入：
//! `touch crates/hub/build.rs` 或 `cargo clean -p anycon-hub`。debug 构建下 rust-embed 直接读磁盘，
//! 只要编译时选中的是 `web/dist`，之后 web 重新构建不需要重编 hub。

use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=static-placeholder");
    println!("cargo:rerun-if-changed=static-placeholder-preview");
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-env-changed=ANYCON_WEB_DIST");
    println!("cargo:rerun-if-env-changed=ANYCON_WEB_PREVIEW_DIST");

    embed(&manifest_dir, "ANYCON_WEB_DIST", "web/dist", "static-placeholder", "ANYCON_WEB_DIST_DIR");
    embed(
        &manifest_dir,
        "ANYCON_WEB_PREVIEW_DIST",
        "web/dist-preview",
        "static-placeholder-preview",
        "ANYCON_WEB_PREVIEW_DIST_DIR",
    );
}

/// 为一份静态目录选定来源，并把选中的路径写进编译期环境变量 `out_var`。
fn embed(manifest_dir: &Path, env_var: &str, repo_dir: &str, placeholder_dir: &str, out_var: &str) {
    let placeholder = manifest_dir.join(placeholder_dir);
    let chosen = pick(manifest_dir, env_var, repo_dir).unwrap_or_else(|| placeholder.clone());
    if chosen != placeholder {
        println!("cargo:rerun-if-changed={}", chosen.display());
    }
    println!("cargo:rustc-env={out_var}={}", chosen.display());
}

fn pick(manifest_dir: &Path, env_var: &str, repo_dir: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(env_var) {
        let p = PathBuf::from(dir);
        if p.join("index.html").is_file() {
            return Some(p);
        }
        println!("cargo:warning={env_var}={} has no index.html, falling back", p.display());
    }
    let dist = manifest_dir.join("../..").join(repo_dir);
    if dist.join("index.html").is_file() {
        return dist.canonicalize().ok();
    }
    None
}
