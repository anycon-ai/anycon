#!/usr/bin/env bash
# `npm pack` / `bun pm pack` / `bun publish` 前自动执行（package.json 的 prepack）：从源码重新构建 tarball 里的东西。
#
#   1. Noise wasm（`just gen-wasm`，需要 wasm32 target 和 wasm-pack）→ wasm/
#   2. wasm 里 Rust 依赖的许可证清单 → wasm/THIRD-PARTY-LICENSES.txt（需要 cargo-about；CI 里缺了直接失败）
#   3. dist/（tsc），先清空，删掉的源文件不会残留在包里
set -euo pipefail

cd "$(dirname "$0")/.."
root=$(cd ../.. && pwd)

(cd "$root" && just gen-wasm)
test -s wasm/anycon_crypto_bg.wasm
# wasm-pack 写的 `*` 会让打包漏掉整个 wasm/（just gen-wasm 已经删了，这里确认一遍）
if [ -e wasm/.gitignore ]; then
  echo "wasm/.gitignore would drop the wasm from the tarball" >&2
  exit 1
fi

if command -v cargo-about >/dev/null 2>&1; then
  (
    cd "$root"
    cargo fetch --locked
    cargo about generate packaging/about.hbs -c packaging/about.toml -m crates/crypto/Cargo.toml --frozen --fail \
      --features wasm --target wasm32-unknown-unknown -o packages/protocol/wasm/THIRD-PARTY-LICENSES.txt
  )
elif [ -n "${CI:-}" ]; then
  echo "cargo-about is required to generate wasm/THIRD-PARTY-LICENSES.txt (cargo install cargo-about --locked --features cli)" >&2
  exit 1
else
  echo "warning: cargo-about not found, the tarball will not include wasm/THIRD-PARTY-LICENSES.txt" >&2
fi

rm -rf dist
bun run build
