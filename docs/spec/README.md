# 功能规格

[architecture.md](../architecture.md) 定了边界，[protocol.md](../protocol.md) 定了消息。这个目录把每个组件**具体做什么**展开到可以直接开工的粒度：路由、子命令、页面、表、状态机、各 agent 的参数映射。

| 文档 | 内容 |
|---|---|
| [daemon.md](daemon.md) | 进程模型、配置、**agent 探测与能力声明**、**会话选项（模型 / 思考 / 权限模式）**、会话生命周期、事件翻译、权限、workspace、存储、多客户端、审计、可靠性 |
| [hub.md](hub.md) | HTTP 路由（登录、OAuth、账号、管理）、WebSocket 行为、配对撮合、设备、通知、配额与限速、数据表、日志纪律 |
| [web.md](web.md) | 路由、三种布局、每个页面、模型 / 思考 / 权限的选择方式、状态层与 IndexedDB、通知、PWA、快捷键、i18n |
| [cli.md](cli.md) | 全部子命令、本机 socket 与经 hub 两条通道、输出约定、`doctor` |
| [sdk.md](sdk.md) | Python / TS SDK 的客户端方向与节点方向 API、错误、发布 |

读的顺序：daemon → web → hub → cli → sdk。daemon 的 § 5 / § 6 是其余几份的共同依赖——客户端不内置任何 agent 知识，全部来自 daemon 的 `agent.list`。

规格和协议文档冲突时以 `crates/protocol` 为准，然后修文档。
