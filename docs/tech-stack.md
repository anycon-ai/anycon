# 技术栈

每一项给出选择和理由。被否掉的备选在 [decisions.md](decisions.md) 里有更完整的记录。

## Rust（hub · daemon · CLI · crypto · protocol）

| 用途 | 选择 | 理由 |
|---|---|---|
| 异步运行时 | tokio | 事实标准 |
| HTTP / WebSocket | axum | tower 中间件生态（限速、超时、压缩、trace 都是现成的），WS 内置，tokio 团队维护。性能差距对本项目无意义——瓶颈在 agent 不在 HTTP 层 |
| 数据库访问 | sqlx | 编译期校验 SQL，自带 migration。不用 `sqlx::Any`，用 trait 抽象双后端（见下） |
| 数据库 | SQLite（WAL） / Postgres | SQLite 是默认，单文件即全部数据；Postgres 给公共 hub 留的 |
| 序列化 | serde + serde_json + rmp-serde | 外层信封 MessagePack，内层 JSON |
| 协议类型导出 | schemars → JSON Schema | 一份 schema 同时生成 TS 和 pydantic；typeshare 只出类型不出 schema，Python 端吃亏 |
| 配置 | figment | TOML + 环境变量合并 |
| CLI | clap（derive） | |
| 日志 | tracing + tracing-subscriber | 结构化，后续接 OpenTelemetry 不改代码 |
| 静态资源嵌入 | rust-embed | Web UI 构建产物打进 hub 二进制 |
| 文件监听 | notify | macOS FSEvents / Linux inotify 统一 |
| Passkey | webauthn-rs | 免密登录，仅 https 域名可用。没有密码、没有 TOTP（ADR-23） |
| OAuth | 自己实现（hyper + hyper-rustls） | GitHub / Google 授权码流程 + PKCE（S256）。拿到 access token 后调 userinfo 取身份，不解析 `id_token`，省掉 JWKS / JWT 校验。只有几步，不引 oauth2 crate（[ADR-40](decisions.md#adr-40-multi-模式开放注册管理员配额账号删除和-oauth-绑定)） |
| HTTP 客户端（出站） | hyper + hyper-util + hyper-rustls | OAuth 换 token、调 userinfo 和 Web Push 共用同一套（`ring` + 内置 webpki 根证书），每次调用有超时、不跟随重定向。不用 reqwest（见 [ADR-37](decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)）；reqwest 只在 hub 的测试里用 |
| 邮件 | lettre | 只走 SMTP。所有邮件服务商都有 SMTP 端点，不接厂商 HTTP API |
| 会话 / 登录 token | 随机 token，库里存 blake2 哈希 | 不用 JWT：要能单独吊销设备，JWT 吊销很别扭。登录 token 是 `single` 模式的零配置登录方式 |
| Noise 协议 | snow | 配对握手和会话加密，不自己拼 x25519 + chacha |
| AEAD | chacha20poly1305 | |
| KDF | argon2（配对短码 → match_id + PSK，64 MiB × 3 轮）+ blake2（指纹、hub 侧哈希） | 短码是唯一的共享秘密、熵低，必须用内存困难的 KDF 抬高恶意 hub 离线穷举的代价（ADR-29） |
| 子进程 | tokio::process | agent 走结构化输出，不用 PTY |
| 错误 | thiserror（库）/ anyhow（二进制） | |
| 测试 | cargo test + insta | 协议序列化适合快照测试 |

### 双后端的做法

```rust
#[async_trait]
pub trait Store: Send + Sync {
    async fn create_device(&self, ...) -> Result<Device>;
    // ...
}
pub struct SqliteStore(Pool<Sqlite>);
pub struct PostgresStore(Pool<Postgres>);
```

两个实现，两套 migration 目录（`migrations/sqlite/`、`migrations/postgres/`），CI 都跑。trait 和 SQLite 实现在 v0.1 完成；Postgres 实现可以晚几周，但抽象层从第一天就有。

### 二进制划分

- `anycon-hub`
- `anycon`：daemon 和 CLI 合一，`anycon daemon` / `anycon pair` / `anycon send` …

用户只装一个东西。

### crypto crate 的三种产物

| 目标 | 工具 |
|---|---|
| Rust | 普通 crate |
| wasm | wasm-bindgen + wasm-pack，产物进 `packages/protocol` |
| Python | PyO3 + maturin，abi3 wheel，一个 wheel 覆盖 ≥ 3.12 |

## Web UI

| 用途 | 选择 | 理由 |
|---|---|---|
| 框架 | React 19 + TypeScript | 熟悉；开源项目找贡献者最容易 |
| 构建 | Vite | 纯静态 SPA，不用 Next.js——需要 Node 运行时的框架没法嵌进 Rust 二进制 |
| 包管理 / 脚本 | bun | workspaces、`bun run`、`bun test` |
| 路由 | TanStack Router | 类型安全 |
| 服务端状态 | TanStack Query | |
| 本地状态 | zustand | |
| 样式 | Tailwind | |
| 组件 | **自己写**，行为层用 Radix Primitives | 视觉自己控制；焦点陷阱、Popover 定位、Combobox 键盘导航这些可访问性细节交给 headless 原语，不自己踩坑 |
| Markdown | react-markdown + shiki | |
| Diff | react-diff-view | |
| 本地缓存 | IndexedDB（idb） | 会话历史缓存 |
| PWA | vite-plugin-pwa | 从第一天就加，手机加图标即可用 |
| 加密 | `crates/crypto` 的 wasm 产物 | |
| 测试 | vitest + Playwright | Playwright 跑 happy path |

**组件约定**：写一页简短的规范（间距刻度、颜色 token、暗色模式切换方式），否则第一个外部 PR 来时风格就散了。

**Web UI 与 SDK 的关系**：Web UI 只依赖 `packages/protocol`（类型 + 编解码），自己写传输和状态层，不依赖 `@anycon/sdk`。两边共享类型，协议一改同时编译报错，不会漂移。

## TypeScript 包

| 包 | 内容 |
|---|---|
| `packages/protocol` | 生成的类型、JSON-RPC 编解码、消息校验、crypto wasm。**无网络、无状态** |
| `packages/sdk`（`@anycon/sdk`） | 连接管理、重连、Promise 风格 API。Node 用 `ws`，浏览器用原生 WebSocket，同构 |

CI 在 **bun 和 Node 两个运行时**上都跑 SDK 测试——用户大多在 Node 上用。

## Python SDK

| 项 | 选择 |
|---|---|
| 最低版本 | **3.12**。3.10 本月 EOL；3.11 的 TaskGroup / ExceptionGroup / tomllib 都要用；3.12 多出 `type` 语句和新泛型语法，生命周期到 2028-10 |
| CI 矩阵 | 3.12 / 3.13 / 3.14；3.15 等生态 wheel 齐了再加 |
| 并发 | asyncio 优先，同步包装器后补 |
| WebSocket | websockets |
| HTTP | httpx |
| 模型 | pydantic v2，从 JSON Schema 生成 |
| 加密 | `crates/crypto` 的 wheel |
| 包管理 | uv（workspace） |
| 格式化 / lint | ruff |
| 测试 | pytest + pytest-asyncio |

## Agent adapter

| agent | 驱动方式 | 备注 |
|---|---|---|
| Claude Code | `claude -p --output-format stream-json --input-format stream-json --include-partial-messages` | 双向 JSONL；权限请求走 `control_request`（官方 Agent SDK 用的同一通道，见 ADR-25）；备选 `PreToolUse` http hook |
| Codex | `codex app-server`（stdio JSON-RPC） | `model/list`、`item/*/requestApproval`、`turn/interrupt`、`turn/steer` 都在这里；`codex exec` 无头模式写死不审批（ADR-26） |
| OpenCode、Pi 及其他 ACP agent | ACP v1 client adapter | 一个 adapter 覆盖多个 agent；模型 / 思考 / 模式来自 `configOptions` |
| 自定义 | Python / TS SDK | agent 本身就是一个节点 |

adapter trait 的语义对齐 [ACP](https://agentclientprotocol.com)。各 CLI 接口变化快，实现时以当时文档为准。

## 构建 · 发布 · 分发

| 用途 | 选择 |
|---|---|
| CI | GitHub Actions |
| 任务入口 | just（`just dev` / `just test` / `just gen`） |
| 工具链版本 | mise |
| Rust 跨平台 release | GitHub Actions 矩阵，原生 runner：macOS arm64/x64、Linux x64/arm64（glibc ≥ 2.35）；安装脚本和 Homebrew formula 在 `packaging/`（ADR-38） |
| macOS 签名 | rcodesign（apple-codesign）：在 Linux CI 上签名 + notarize，不需要 Mac runner |
| Docker | multi-arch（amd64 + arm64，各在原生 runner 上构建），distroless 基础镜像，目标几十 MB，推到 GHCR |
| 版本与 changelog | 手工维护 `CHANGELOG.md`（按 conventional commits 整理） |
| 第三方许可证 | cargo-about（`packaging/about.toml`）+ Web UI 构建时收集 npm 包的许可证，随每个产物分发 |
| npm 发布 | 随版本标签发布 `packages/protocol` 和 `packages/sdk`：bun 打包（解析 `workspace:*`），npm 发布 tarball（带 provenance） |
| PyPI 发布 | maturin（crypto wheel）+ uv build（纯 Python 包） |

## 测试策略

| 层 | 做法 |
|---|---|
| 协议 | insta 快照；生成产物与源码一致性检查 |
| 加密 | 三端握手互通测试（Rust ↔ wasm ↔ Python） |
| 兼容矩阵 | `hub(N) × daemon(N-1)`、`daemon(N) × client(N-1)` |
| hub | 集成测试起真实 hub + 两个模拟设备 |
| daemon | adapter 用录制的事件流回放，不依赖真实 agent CLI |
| Web | Playwright：配对 → 建项目 → 发消息 → 审批 |
