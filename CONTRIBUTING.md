# Contributing

欢迎贡献。动手之前先读 [docs/architecture.md](docs/architecture.md)（组件与数据归属）和 [docs/spec/](docs/spec/)（每个模块具体做什么）；协议细节在 [docs/protocol.md](docs/protocol.md)，设计取舍在 [docs/decisions.md](docs/decisions.md)。较大的改动请先开 issue 讨论。

## 仓库结构

| 目录 | 内容 |
|---|---|
| `crates/protocol` | 协议类型的唯一真源，导出 JSON Schema（→ `schema/`、`packages/protocol/src/generated.ts`） |
| `crates/agent-api` | adapter trait：daemon 和各 agent 之间的接口 |
| `crates/crypto` | Noise 握手与加密、CPace；同一份实现编译成 Rust / wasm（→ `packages/protocol/wasm`，生成产物不进仓库） |
| `crates/hub` | `anycon-hub` 二进制；`docker/` 是它的镜像和 compose 示例 |
| `crates/daemon` | daemon 核心：会话、workspace、worktree、权限、存储 |
| `crates/adapters` | Claude Code / Codex / mock adapter |
| `crates/cli` | `anycon` 二进制（daemon + CLI 合一）；`scripts/dev.sh` 是 `just dev` 的实现 |
| `web/` | Web UI（React + Vite），构建产物嵌入 hub |
| `packages/protocol` | 生成的 TS 类型、信封 / JSON-RPC 编解码、wasm 封装 |
| `packages/sdk` | `@anycon/sdk` |
| `e2e/` | Playwright 端到端 |
| `packaging/` | 发布用：安装脚本、Homebrew formula 生成脚本、cargo-about 的第三方许可证配置（见「发布」） |
| `sdk/python/` | Python SDK 占位（代码在 v0.1.x） |

跨多个模块的改动在 PR 描述里说明影响到哪些模块。

## 本地开发

需要的工具版本在 `mise.toml`（Rust、bun、Node、just）。Rust 测试里的假 agent 是 Python 脚本、有一个测试用 perl，所以还需要系统的 `python3` 和 `perl`。

```bash
mise install                 # 或自己装齐 mise.toml 里的版本
bun install
just gen-wasm                # crypto wasm → packages/protocol/wasm
bun run --cwd web build      # hub 编译时嵌入 web/dist
just dev                     # hub :8080 + daemon（mock agent）+ web :5173，终端打印 admin token
```

浏览器打开 `http://localhost:5173`（用 `localhost`，不要用 `127.0.0.1`），粘贴 token，另开终端 `ANYCON_HOME=.anycon-dev/daemon ./target/debug/anycon pair`，把短码输入网页。mock agent 的行为表在 [crates/adapters/src/mock/README.md](crates/adapters/src/mock/README.md)；`DEV_REAL_CLAUDE=1 just dev` 换成真实 Claude Code。`rm -rf .anycon-dev` 重置。

hub 只嵌入它**编译时**看到的 `web/dist`：先编了 hub、后构建 web 的话，hub 还在下发占位页，`touch crates/hub/build.rs` 重编一次即可（或设 `ANYCON_WEB_DIST=<绝对路径>/web/dist`）。

## 提交前

- `just lint`：`cargo fmt --check`、`cargo clippy -D warnings`、oxlint、oxfmt、tsc
- `just test`：Rust 全部 + TS 各包
- 改了 `crates/protocol` 必须 `just gen` 并一起提交 `schema/`、`packages/protocol/src/generated.ts` 和 `packages/protocol/src/method-types.ts`（方法的 params / result 类型由 `schema/methods.json` 生成），CI 用 `just gen-check` 检查
- 端到端：`just e2e`（Playwright，真实 hub + daemon(mock) + web；端口随机，可以和别的 e2e 同时跑）、`just test-sdk-real`（3–4 分钟）、`just test-happy-path`。改了配对 / 加密 / 消息大小相关的东西，e2e 里有对应用例，见 [e2e/README.md](e2e/README.md)
- 真实 agent CLI 的测试放在 `ANYCON_REAL_AGENT_TESTS=1` 后面，会花你的 token
- 用 npm 镜像源装过依赖的话，提交前跑 `just lock-clean`：CI 拒绝 `bun.lock` 里出现非默认 registry 的地址

## 约定

- Rust：`thiserror` 做库错误、`anyhow` 只在二进制；`tracing` 字段化日志（`ANYCON_LOG` / `RUST_LOG` 过滤）；**hub 代码里不出现任何会话内容**：不解析 payload、不写日志、不存
- TS：严格模式，无 `any`；组件自己写，行为层用 Radix；文案走 i18n（en / zh）
- 测试和代码放一起（Rust `#[cfg(test)]`、TS `*.test.ts`），集成测试放 `tests/`。测试不依赖网络，不碰真实的 `~/.anycon`（用临时目录）；会起的外部进程只有 `git` 和 mock agent
- 时间一律 Unix 毫秒 `u64`，字段名以 `_ms` 结尾。id：设备 32 位小写 hex；会话 `sess_` + 22 位 base62（worktree `wt_` + 22 位）；request / message / job 是 uuid v4 字符串；turn 是会话内从 1 开始的 `u64`
- RPC 错误用 `RpcError::new(ErrorCode::…)`；客户端只依赖 `data.code`（稳定的 snake_case）
- 数据库迁移只前进：发布之后不改已有的迁移文件，加新的 `NNNN_<名字>.sql`
- commit 信息用 conventional commits；面向用户的变化记进 [CHANGELOG.md](CHANGELOG.md)
- 新增设计决策写进 `docs/decisions.md`，推翻旧决策不删条目，加「已被 ADR-N 取代」
- 注释和文档直接写事实和理由；需要出处时引用 `docs/` 里的章节或 ADR
- 写安全 / 隐私相关的文字要精确：说「hub 只看到元数据」「Web UI 的代码由 hub 下发」，不要写「hub 永远看不到内容」这类绝对表述（[ADR-32](docs/decisions.md#adr-32-如实声明web-客户端代码由-hub-下发)）。改了威胁模型要同时改 `docs/architecture.md § 5.6`、两份 README 的隐私模型和 `SECURITY.md`
- 文档里不写竞品名字，只写方法

## 发布

由 [`.github/workflows/release.yml`](.github/workflows/release.yml) 完成，设计见 [ADR-38](docs/decisions.md#adr-38-发布原生-runner-上的矩阵构建不用-cargo-dist)。推一个 `v*` 标签，它构建并自检 hub 镜像、四个平台的 `anycon`、两个 npm 包，全部通过后打镜像标签、建 GitHub Release（压缩包、`install.sh`、`SHA256SUMS`、构建来源证明）、发 npm、更新 Homebrew tap。

1. 版本号三处一致（流水线会检查）：根 `Cargo.toml` 的 `[workspace.package] version`、`packages/protocol/package.json`、`packages/sdk/package.json`；`cargo check` 更新 `Cargo.lock`。`CHANGELOG.md` 写好这一版的段落，把 `Unreleased` 换成日期（Release 的说明取自这一段）
2. 可选：在 Actions 页面手动运行 Release。手动运行只构建和自检、不发布，产物在 run 的 artifacts 里
3. `git tag v0.1.0 && git push origin v0.1.0`。版本号带 `-`（如 `0.2.0-rc.1`）的是预发布：镜像只打版本号标签、npm 用 `next`、不更新 Homebrew

第一次发布前要手动准备（之后不用再做）：

- **仓库公开**：npm provenance、公开的 GHCR 镜像、Homebrew 和安装脚本下载 Release 文件都要求仓库是公开的
- **npm**：建 `anycon` 组织（`@anycon` scope）；生成一个能发布 `@anycon/*` 的 granular access token（账号开了 2FA 的话要允许它绕过 2FA），存成仓库 secret `NPM_TOKEN`。granular token 有有效期，过期前换掉；也可以在包发布过一次之后，在 npm 的包设置里给 `anycon-ai/anycon` 的 `release.yml` 配 trusted publishing，之后不需要 token
- **Homebrew**：在 `anycon-ai` 下建公开仓库 `homebrew-tap`，建的时候勾上「添加 README」（要有默认分支；流水线往里写 `Formula/anycon.rb`）；建一个只对这个仓库有 Contents 读写权限的 fine-grained token，存成 secret `HOMEBREW_TAP_TOKEN`
- **GHCR**：组织设置里允许公开的 package。第一次发布推上去的 `ghcr.io/anycon-ai/hub` 默认是私有的：到组织的 Packages → `hub` → Package settings 改成 Public，确认它关联到了 `anycon` 仓库（镜像的 `org.opencontainers.image.source` 会自动关联）。改之前 `docker pull` 要登录

## 现在最需要的

- 读 [docs/spec/](docs/spec/) 和代码对照，在 issue 里挑规格和实现不一致的地方
- 对 [decisions.md](docs/decisions.md) 里的任何一条提出反对，带理由
- 在你的机器 / 发行版上跑一遍 `just dev` 和 `just e2e`，报告环境问题（Linux 上的 systemd 用户服务还没有在实机上验证过）
- 新的 agent adapter（见 [roadmap](docs/roadmap.md) 的 ACP adapter）
