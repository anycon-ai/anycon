# anycon-hub 镜像：自包含的多阶段构建（wasm → Web UI → hub），运行时用 distroless（无 shell、无 curl），非 root。
# 不需要在宿主机先构建任何东西：Noise wasm、`web/dist`、hub 二进制都在镜像构建里完成，Web UI 被嵌进二进制。
#
# 构建（在仓库根目录）：
#   docker build -f docker/hub.Dockerfile -t anycon-hub:dev .     # 或 just docker-hub
#
# 阶段：
#   wasm-tools   rustup wasm32 target + 与 Cargo.lock 里 wasm-bindgen 版本一致的 wasm-bindgen-cli（只在版本变时重建）
#   wasm         crates/crypto/scripts/build-wasm.sh → packages/protocol/wasm
#   web          bun install --frozen-lockfile && vite build → web/dist（缺 wasm 时直接失败，不会产出残缺的 UI）
#   builder      cargo build --release -p anycon-hub，ANYCON_WEB_DIST 指向上一步的 dist（crates/hub/build.rs 嵌入）
#   licenses     cargo-about 生成 hub 二进制和 Noise wasm 的 Rust 依赖许可证清单（packaging/about.toml）
#   最终镜像      distroless/cc-debian12:nonroot；许可证在 /usr/share/doc/anycon-hub/
#
# 基础镜像、npm 源都是构建参数；CI 用默认值，Docker Hub / npm 访问不稳的机器本地换源：
#   docker build -f docker/hub.Dockerfile -t anycon-hub:dev \
#     --build-arg RUST_IMAGE=<镜像源>/library/rust:1-bookworm \
#     --build-arg BUN_IMAGE=<镜像源>/oven/bun:1.4 \
#     --build-arg NPM_REGISTRY=<npm 镜像源 URL> .
#
# 供应链：三个基础镜像的默认值都**钉了 digest**（多架构 index 的 digest，amd64 / arm64 通用），
# 浮动标签（`rust:1-bookworm` 等）被上游改写时不会悄悄换掉构建环境。覆盖时可以不带 digest（上面的镜像源例子），
# 但那样就失去了固定；要固定镜像源里的镜像，加上 `@sha256:<同一个 digest>`——镜像源转发的是同一份内容，digest 一致。
# 升级基础镜像时（建议随 Rust / bun 的小版本更新做一次）：
#   docker buildx imagetools inspect rust:1-bookworm | head -3          # 取新的 Digest，替换下面的 @sha256:…
#   （同样适用于 oven/bun:1.4 和 gcr.io/distroless/cc-debian12:nonroot）
# 运行时镜像是 distroless nonroot（uid 65532），没有 shell、没有包管理器；下面 `USER 65532:65532` 保持非 root。
#
# 运行：
#   docker run -d --name anycon-hub -p 127.0.0.1:8080:8080 \
#     -v anycon-data:/data -e ANYCON_PUBLIC_URL=https://hub.example.com anycon-hub:dev
#
# 数据卷 /data 由 uid 65532（nonroot）写入。用命名卷即可开箱即用；
# 用宿主机目录（bind mount）时要先 `chown -R 65532:65532 <dir>`。

# 钉 digest 的日期：2026-10-11（标签 → digest 的对应关系见上面的升级说明）。
ARG RUST_IMAGE=rust:1-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6
ARG BUN_IMAGE=oven/bun:1.4@sha256:ec06c3b6cea04192ae6770c434f668ca41d343ad19fa6472216c7b48be39c598
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

# cargo 的 registry / git 缓存挂载一律 sharing=locked：wasm 和 hub 两条构建线会并行跑，
# 默认的 shared 会让两个 cargo 同时解包同一个 crate，报 "File exists (os error 17)"。
# ---------- wasm 工具 ----------
# 先单独取出 wasm-bindgen 的版本号再装 CLI：只有版本变了这一层（编译约几分钟）才会失效，
# Cargo.lock 的其它改动不会让它重来。
FROM ${RUST_IMAGE} AS wasm-version
COPY Cargo.lock /Cargo.lock
RUN grep -A1 '^name = "wasm-bindgen"$' /Cargo.lock | sed -n 's/^version = "\(.*\)"$/\1/p' | head -1 > /wasm-bindgen-version \
    && test -s /wasm-bindgen-version

FROM ${RUST_IMAGE} AS wasm-tools
RUN rustup target add wasm32-unknown-unknown
COPY --from=wasm-version /wasm-bindgen-version /wasm-bindgen-version
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo install wasm-bindgen-cli --locked --version "$(cat /wasm-bindgen-version)"

# ---------- wasm：Noise（crypto crate）→ packages/protocol/wasm ----------
FROM wasm-tools AS wasm
WORKDIR /src
# 工作区需要所有成员的 manifest 才能解析，所以整个 crates/ 都复制；只会编译 anycon-crypto 及其依赖。
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,id=anycon-wasm-target \
    CARGO_TARGET_DIR=/src/target crates/crypto/scripts/build-wasm.sh \
    && test -s packages/protocol/wasm/anycon_crypto_bg.wasm

# ---------- Web UI：bun + vite ----------
FROM ${BUN_IMAGE} AS web
WORKDIR /src
# 先只放 manifest 装依赖：源码改动不会让 `bun install` 这一层失效。
COPY package.json bun.lock tsconfig.base.json ./
COPY packages/protocol/package.json packages/protocol/package.json
COPY packages/sdk/package.json packages/sdk/package.json
COPY e2e/package.json e2e/package.json
COPY web/package.json web/package.json
# 默认 registry 为空；换源：--build-arg NPM_REGISTRY=<npm 镜像源 URL>
ARG NPM_REGISTRY=
RUN env ${NPM_REGISTRY:+BUN_CONFIG_REGISTRY=$NPM_REGISTRY} bun install --frozen-lockfile --filter @anycon/web
COPY packages/protocol packages/protocol
COPY --from=wasm /src/packages/protocol/wasm packages/protocol/wasm
COPY web web
# web/vite.config.ts 在 wasm 缺失时会换成运行时才报错的桩，所以这里显式确认真的 wasm 进了产物。
RUN test -s packages/protocol/wasm/anycon_crypto.js \
    && bun run --cwd web build \
    && ls web/dist/assets/*.wasm \
    && test -s web/dist/index.html \
    && test -s web/dist/THIRD-PARTY-LICENSES.txt

# ---------- hub ----------
FROM ${RUST_IMAGE} AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY --from=web /src/web/dist /web-dist
# crates/hub/build.rs：ANYCON_WEB_DIST 优先于 web/dist；目录里没有 index.html 会回退到占位页，所以下面再校验一次。
ENV ANYCON_WEB_DIST=/web-dist
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,id=anycon-hub-target \
    cargo build --release --locked -p anycon-hub \
    && cp target/release/anycon-hub /anycon-hub \
    && mkdir /empty-data \
    && test -s /web-dist/index.html \
    && ! grep -qa "this build does not include the web UI" /anycon-hub

# ---------- 第三方许可证 ----------
# hub 二进制和嵌进 Web UI 的 Noise wasm 各一份（Web UI 里 npm 包的清单由 vite 构建生成，见 web/scripts/third-party-licenses.ts）。
# cargo-about 的版本钉死，只有版本变了这一层才重新编译。
FROM ${RUST_IMAGE} AS about-tools
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo install cargo-about --locked --features cli --version 0.9.2

FROM about-tools AS licenses
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY packaging/about.toml packaging/about.hbs ./packaging/
# 先 fetch，cargo-about 再离线跑：许可证原文从 crate 源码包里读，不去 GitHub 抓。
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo fetch --locked \
    && mkdir /licenses \
    && cargo about generate packaging/about.hbs -c packaging/about.toml -m crates/hub/Cargo.toml --frozen --fail \
         --target x86_64-unknown-linux-gnu --target aarch64-unknown-linux-gnu -o /licenses/THIRD-PARTY-LICENSES.txt \
    && cargo about generate packaging/about.hbs -c packaging/about.toml -m crates/crypto/Cargo.toml --frozen --fail \
         --features wasm --target wasm32-unknown-unknown -o /licenses/THIRD-PARTY-LICENSES-wasm.txt

# ---------- 运行 ----------
FROM ${RUNTIME_IMAGE}

# org.opencontainers.image.source 让 GHCR 把镜像关联到仓库；发布流水线（.github/workflows/release.yml）还会补上版本、提交等标签。
LABEL org.opencontainers.image.title="anycon-hub" \
      org.opencontainers.image.description="Anycon hub: routes end-to-end encrypted sessions between your machines and clients, and serves the Web UI" \
      org.opencontainers.image.source="https://github.com/anycon-ai/anycon" \
      org.opencontainers.image.licenses="Apache-2.0"

COPY --from=builder /anycon-hub /usr/local/bin/anycon-hub
COPY --chmod=0644 LICENSE /usr/share/doc/anycon-hub/LICENSE
COPY --from=licenses /licenses/ /usr/share/doc/anycon-hub/
COPY --from=web /src/web/dist/THIRD-PARTY-LICENSES.txt /usr/share/doc/anycon-hub/THIRD-PARTY-LICENSES-web.txt
# distroless 里没有 shell 可以 mkdir/chown，所以在这里把 /data 建好并交给 nonroot（65532）。
# 之后 `VOLUME` 让新建的命名卷继承这个属主。
COPY --from=builder --chown=65532:65532 /empty-data /data

ENV ANYCON_DATA_DIR=/data \
    ANYCON_LISTEN=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
USER 65532:65532
WORKDIR /data

# 镜像里没有 curl：健康检查是二进制自带的子命令（对 127.0.0.1:<ANYCON_LISTEN 端口>/healthz 发 GET）。
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/anycon-hub", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/anycon-hub"]
