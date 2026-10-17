# anycon 任务入口。`just` 列出全部。

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# ---------- 生成 ----------

# 协议 schema → TS / Python 类型。改了 crates/protocol 必须跑，CI 会 diff 检查。
gen: gen-schema gen-ts gen-py

gen-schema:
    cargo run -q -p anycon-protocol --bin anycon-gen-schema -- schema/protocol.json

# generated.ts（schema/protocol.json）+ method-types.ts（schema/methods.json 的方法注册表）
gen-ts:
    cd packages/protocol && bunx --yes json-schema-to-typescript@15 ../../schema/protocol.json --no-additionalProperties --bannerComment $'/* eslint-disable */\n// 由 `just gen` 从 schema/protocol.json 生成，不要手改。' -o src/generated.ts
    bun packages/protocol/tools/gen-method-types.ts

gen-py:
    @echo "python 类型生成在 v0.1 之后（sdk/python 目前只有骨架）"

# 优先用本机的 wasm-pack（cargo install wasm-pack --locked），没有才退到 `bunx wasm-pack@0.13`
# （npm 包首次运行会从 GitHub 下载二进制，该下载不走 HTTP(S)_PROXY，在需要代理的机器上会失败）。
# 两者都不想用时：`crates/crypto/scripts/build-wasm.sh`（只需要 wasm32 target 和与 Cargo.lock 版本一致的
# wasm-bindgen-cli，产物文件名相同）。
# crypto wasm → packages/protocol/wasm
gen-wasm:
    #!/usr/bin/env bash
    set -euo pipefail
    cd crates/crypto
    if command -v wasm-pack >/dev/null 2>&1; then
        wasm-pack build --target web --out-dir ../../packages/protocol/wasm --out-name anycon_crypto --release -- --features wasm
    else
        bunx --yes wasm-pack@0.13 build --target web --out-dir ../../packages/protocol/wasm --out-name anycon_crypto --release -- --features wasm
    fi
    # wasm-pack 会写一个内容为 `*` 的 wasm/.gitignore，npm / bun 打包时会因此把 wasm 整个漏掉。
    # 仓库根的 .gitignore 已经忽略了这个目录，这份删掉不影响 git。
    rm -f ../../packages/protocol/wasm/.gitignore

# 用 Rust 真实编码器重新生成 TS 编解码的字节级测试向量（packages/protocol/tests/vectors）。协议的信封 / 控制消息 / JSON-RPC 编码变了才需要跑。
gen-vectors:
    CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}" cargo run -q --manifest-path packages/protocol/tools/gen-vectors/Cargo.toml

gen-check: gen
    git diff --exit-code -- schema packages/protocol/src/generated.ts packages/protocol/src/method-types.ts

# ---------- 构建 / 测试 ----------

# 先 wasm 和 web，后 Rust：hub 的 build.rs 在编译时选中 web/dist 嵌入。
build: gen-wasm
    bun install
    bun run build
    cargo build --workspace

test: test-rust test-ts

test-rust:
    cargo test --workspace

# TS 单元测试（bun）；e2e 不在里面，用 `just e2e`。Node 运行时：bun run --cwd packages/sdk test:node
test-ts:
    bun install
    bun run test

# 需要 wasm（先 `just gen-wasm`）；二进制路径可用 ANYCON_BIN_DIR 覆盖（默认 target/debug）。
# SDK 对真实 hub + daemon（mock agent）的端到端（packages/sdk/tests/real.e2e.test.ts）
test-sdk-real:
    cargo build -p anycon-hub -p anycon-cli
    bun install
    cd packages/sdk && ANYCON_SDK_REAL=1 bun test tests/real.e2e.test.ts

# Rust 端到端：进程内 hub + 真实 daemon（mock agent）+ CLI 客户端（connect / new / approve / history / cancel）。
test-happy-path:
    cargo test -p anycon-cli --test happy_path

# bun 会把非默认 registry 的 tarball 地址写进 bun.lock（比如本机装依赖用了镜像）；提交前用它清掉（空串 = 默认 registry）。
lock-clean:
    #!/usr/bin/env python3
    import re
    s = open("bun.lock").read()
    t = re.sub(r'"https://registry\.(?!npmjs\.org)[^"/]+/[^"]*"', '""', s)
    open("bun.lock", "w").write(t)
    print("bun.lock: cleaned" if s != t else "bun.lock: already clean")

lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    # python 绑定是 feature，workspace 的 clippy 不编它
    cargo clippy -p anycon-crypto --all-targets --features python -- -D warnings
    bun run lint
    bun run fmt:check
    bun run typecheck

fmt:
    cargo fmt --all
    bun run fmt

# ---------- 开发 ----------

# 起 hub（:8080）+ daemon（mock agent）+ web dev server（:5173）。选项见 scripts/dev.sh 顶部注释。
dev:
    ./scripts/dev.sh

# 前置：cargo build -p anycon-hub -p anycon-cli，以及 just gen-wasm && bun run --cwd web build（见 e2e/README.md）。
# Chromium 下载走 CDN；连不上时设 PLAYWRIGHT_DOWNLOAD_HOST 指向镜像。
e2e:
    bun install
    cd e2e && bunx playwright install chromium && bun run test

# Docker Hub / npm 连不上的机器本地换源（CI 不设，用 Dockerfile 里的默认值）：
#   RUST_IMAGE=<镜像源>/library/rust:1-bookworm BUN_IMAGE=<镜像源>/oven/bun:1.4 \
#   NPM_REGISTRY=<npm 镜像源 URL> just docker-hub
# 构建 hub 镜像（自包含：wasm、Web UI、hub 都在镜像里构建，docker/hub.Dockerfile）
docker-hub *args:
    #!/usr/bin/env bash
    set -euo pipefail
    build_args=()
    for v in RUST_IMAGE BUN_IMAGE RUNTIME_IMAGE NPM_REGISTRY; do
        if [ -n "${!v:-}" ]; then build_args+=(--build-arg "$v=${!v}"); fi
    done
    docker build -f docker/hub.Dockerfile -t anycon-hub:dev ${build_args[@]+"${build_args[@]}"} {{args}} .
