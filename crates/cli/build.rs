//! Windows 的主线程栈默认只有 1 MiB（macOS / Linux 是 8 MiB）。`anycon` 在主线程上 `block_on` 整个命令（包括 daemon），
//! 调试构建里的大 future 很容易把它撑爆（`STATUS_STACK_OVERFLOW`）。链接时把 Windows 上的主线程栈设成 8 MiB，和另外两个平台一致。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let arg = match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => "/STACK:8388608",
        _ => "-Wl,--stack,8388608",
    };
    println!("cargo:rustc-link-arg-bin=anycon={arg}");
}
