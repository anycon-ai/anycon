# anycon (Python SDK) — 推迟，v0.1 没有代码

这个目录只是占位。**v0.1 不包含 Python SDK**：没有可安装的包，`pip install anycon` 现在装不到任何东西（PyPI 上的名字在发布前先占位）。

## 状态

| 项 | 状态 |
|---|---|
| `anycon-crypto`（Noise，PyO3 abi3 wheel） | 只有 `crates/crypto/src/python.rs` 的骨架（`python` feature），没有 wheel |
| 类型生成（pydantic，`schema/protocol.json` → `anycon/_types.py`） | 未做，见 `docs/protocol.md` § 10 |
| 客户端方向（`anycon.connect` / `sessions.create` / `send` …） | 未做 |
| 节点方向（`@anycon.agent`，把自己的 agent 注册为节点） | 未做 |

要用 SDK 现在请用 TypeScript 的 `@anycon/sdk`（`packages/sdk`），它覆盖客户端方向，并有对真实 hub + daemon 的端到端测试（`just test-sdk-real`）。

## 将来的样子

API 设计在 [`docs/spec/sdk.md`](../../docs/spec/sdk.md)（§ 3.1 客户端方向、§ 4 节点方向）。按路线图排在 v0.1.x：先 wheel 和类型生成，再客户端方向，最后节点方向，见 [`docs/roadmap.md`](../../docs/roadmap.md) 的 v0.1.x。

目录布局（预留，尚不存在）：

```
sdk/python/
├── pyproject.toml        # 本文件旁边的占位
├── anycon/               # 纯 Python 包：连接管理、重连、async API、事件流
│   └── _types.py         # 从 schema/protocol.json 生成
└── tests/
```

## 贡献

如果你想做这件事：先读 `docs/spec/sdk.md`、`docs/protocol.md` 和 `crates/crypto/README.md`；配对流程（短码本地使用、`match_id`、`pair.ack` 校验、分片）和 `packages/sdk` 保持逐行一致，TS 版是参照实现。
