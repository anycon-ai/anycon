# anycon — 给 coding agent 的工作约定

先读 `docs/architecture.md`（组件与数据归属）、`docs/spec/`（功能规格）和 `docs/protocol.md`；设计取舍和被否掉的方案在 `docs/decisions.md`。贡献流程见 `CONTRIBUTING.md`。

## 不可违反

- `crates/protocol` 是唯一真源。改了它必须 `just gen` 并一起提交 `schema/`、`packages/protocol/src/generated.ts` 和 `packages/protocol/src/method-types.ts`。不要在别处手写重复的类型。
- 依赖加在各自 crate 里：`cargo add -p <crate> <dep>`。根 `Cargo.toml` 的 `[workspace.dependencies]` 只放多个 crate 共用的依赖。
- hub 代码里不出现任何会话内容：不解析 payload、不写日志、不存。这是 review 项。
- 调用外部命令（git、agent CLI 等）都带超时。测试不依赖网络；真实 agent CLI 的测试放在 `#[ignore]` 或环境变量 `ANYCON_REAL_AGENT_TESTS=1` 后面。
- 安全 / 隐私相关的文字要精确（ADR-32）：说「hub 只看到元数据」「Web UI 的代码由 hub 下发」，不写「hub 永远看不到内容」。改了威胁模型要同时改 `docs/architecture.md § 5.6`、两份 README 的隐私模型和 `SECURITY.md`。

## 命令

- Rust：`cargo check -p <crate>`、`cargo test -p <crate>`、`cargo clippy -p <crate> --all-targets -- -D warnings`、`cargo fmt`
- TS：根目录 `bun install`；`bun run --cwd <pkg> test|lint|typecheck|build`
- 生成：`just gen`（schema + TS）、`just gen-wasm`
- 全部：`just test`、`just lint`、`just e2e`

## 风格

- Rust：`thiserror` 做库错误，`anyhow` 只在二进制；`tracing` 字段化日志；每个公开模块顶部一段中文 `//!` 说明职责；注释用中文，标识符用英文。
- CLI 的 clap `///` 文档注释就是 `--help` 文字，面向用户，用英文写；CLI 打印给用户的文字也用英文。其余注释照常用中文。
- TS：严格模式，无 `any`（必要时 `unknown` + 收窄）；组件自己写，行为层用 Radix；文案走 i18n（en / zh）。
- 测试和代码放一起（Rust `#[cfg(test)]`、TS `*.test.ts`）。集成测试放 `tests/`。
- 注释和文档直接写事实和理由，不引用开发过程里的轮次、任务编号或验证报告编号；需要出处时引用 `docs/` 里的章节或 ADR。
- 数据库迁移只前进：发布之后不改已有的迁移文件，加新的 `NNNN_<名字>.sql`。
- 提交信息用 conventional commits。
