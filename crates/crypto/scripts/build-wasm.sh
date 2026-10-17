#!/usr/bin/env bash
# 手动构建 wasm 产物到 packages/protocol/wasm/（等价于 `just gen-wasm`，但不依赖 wasm-pack / npm / GitHub）。
#
# 依赖：rustup target wasm32-unknown-unknown，以及与 Cargo.lock 里 wasm-bindgen 版本一致的 `wasm-bindgen` CLI：
#   cargo install wasm-bindgen-cli --version "$(grep -A1 'name = "wasm-bindgen"' ../../Cargo.lock | sed -n 's/version = "\(.*\)"/\1/p')" --locked
# 可选：`wasm-opt`（binaryen）在 PATH 里时会再压一遍体积。
#
# 产物（--target web）：anycon_crypto.js / anycon_crypto_bg.wasm / anycon_crypto.d.ts / anycon_crypto_bg.wasm.d.ts
set -euo pipefail

cd "$(dirname "$0")/.."
crate_dir=$PWD
root=$(cd ../.. && pwd)
out="$root/packages/protocol/wasm"

want=$(grep -A1 'name = "wasm-bindgen"' "$root/Cargo.lock" | sed -n 's/version = "\(.*\)"/\1/p' | head -1)
have=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)
if [ "$have" != "$want" ]; then
  echo "need wasm-bindgen CLI $want (found: ${have:-none}); run: cargo install wasm-bindgen-cli --version $want --locked" >&2
  exit 1
fi

cargo build -p anycon-crypto --release --features wasm --target wasm32-unknown-unknown

target_dir=${CARGO_TARGET_DIR:-$root/target}
wasm="$target_dir/wasm32-unknown-unknown/release/anycon_crypto.wasm"

rm -rf "$out"
mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" --out-name anycon_crypto "$wasm"

if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int -o "$out/anycon_crypto_bg.wasm.opt" "$out/anycon_crypto_bg.wasm"
  mv "$out/anycon_crypto_bg.wasm.opt" "$out/anycon_crypto_bg.wasm"
fi

ls -l "$out"
cd "$crate_dir"
