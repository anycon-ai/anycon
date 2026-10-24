<p align="center">
  <img src="web/public/logo.svg" width="88" alt="">
</p>

<h1 align="center">Anycon</h1>

<p align="center">
  coding agent 跑在你自己的机器上，从任何地方和它对话。
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License: Apache-2.0"></a>
  <img src="https://img.shields.io/badge/status-v0.1-orange" alt="Status: v0.1">
  <a href="README.md">English</a>
</p>

---

Anycon 把跑 AI agent 的那台机器（家里的 Mac mini、工作站、服务器）接到一个叫 **hub** 的小中继上，然后你可以从浏览器、手机、终端或脚本，在任何地方驱动这些 agent。机器主动出站连 hub：不需要公网 IP，不需要 VPN，不需要端口转发。

```
     your machine                       hub                             clients
┌───────────────────┐             ┌──────────────┐                  ┌──────────────┐
│ anycon daemon     │  outbound   │ anycon-hub   │   HTTPS / WS     │ Web UI / PWA │
│ ├ Claude Code     │ ──────────► │ relays       │ ◄─────────────── │ anycon CLI   │
│ └ Codex, …        │             │ ciphertext   │                  │ TS SDK       │
└───────────────────┘             └──────────────┘                  └──────────────┘
          ▲                                                                 ▲
          └───────────────── end-to-end encrypted (Noise) ──────────────────┘
```

## 为什么

厂商的云端 agent 跑在厂商的沙箱里：代码克隆进容器，依赖重装，内网服务够不到。Anycon 让 agent 留在**你自己的**机器上——本地数据库、GPU、凭据、装好的工具都在——外面只是一条线。

- **自建优先**。hub 就是一个容器加一个 SQLite 文件，没有外部依赖
- **hub 只看到元数据**。会话历史在你的机器上；客户端和 daemon 之间是 Noise 端到端加密，hub 只转发密文、只看到元数据（唯一的大例外见[隐私模型](#隐私模型)）
- **agent 无关**。Claude Code、Codex 开箱即用，其它 agent 通过 adapter 接口接入
- **文件夹即项目**。workspace 目录下每个文件夹就是一个项目；在任何客户端都能新建、克隆，或者关联机器上已有的目录
- **真正的编码工作流**。每个会话可以有自己的 git worktree 和分支：看 diff、同步 base、审批权限请求、让 agent 提交和开 PR——这些都在你的机器上做
- **协议优先**。Web UI、CLI、SDK 走同一套公开协议

代价也说清楚：

- **没有隔离**。agent 以你的用户身份跑在你的机器上，workspace 边界靠 agent 自己的权限审批，不是 OS 级沙箱
- **机器要常开**，联网、备份都是你的事
- **模型 API 照样看得到上下文**。你机器上的 Claude Code 依然把提示发给它的模型提供方，Anycon 不改变这一点

**不做**：终端和远程 shell、编辑器、issue 跟踪、团队协作。dev server 预览走端到端加密的隧道，不经 hub 明文反代（见隐私模型）。

## 快速开始

hub 需要 Docker。`anycon`（daemon 和 CLI 是同一个二进制）支持 macOS（Apple 芯片和 Intel）和 Linux（x86_64 和 arm64，glibc 2.35 及以上）。

**1. 启动 hub**（可以和 agent 在同一台机器上，也可以放在两边都能访问的任何地方）

```bash
docker run -d --name anycon-hub -p 8080:8080 -v anycon-data:/data \
  -e ANYCON_PUBLIC_URL=https://hub.example.com ghcr.io/anycon-ai/hub:latest
docker logs anycon-hub    # 首次启动会打印一次管理员 token
```

想让别人自助注册（邮箱验证码或 GitHub / Google 登录，带配额）？用 `ANYCON_MODE=multi`，见 [docs/deploy.md § 9](docs/deploy.md#9-公共-hub-与-multi-模式)。

镜像有 `linux/amd64` 和 `linux/arm64` 两种架构，标签是 `<版本>`（如 `0.1.0`）、`<主>.<次>`（`0.1`）和 `latest`。生产环境请钉住版本。hub 只提供明文 HTTP，TLS 和域名交给你的反向代理。[docs/deploy.md](docs/deploy.md) 有 Caddy / nginx / Traefik / Cloudflare Tunnel 的现成配置，还有一份 [compose 文件](docker/compose.yml)。

**2. 在跑 agent 的机器上安装并配对 daemon**

```bash
brew install anycon-ai/tap/anycon
# 不用 Homebrew 的话（装到 ~/.local/bin）：
curl -fsSL https://github.com/anycon-ai/anycon/releases/latest/download/install.sh | sh

anycon init --hub https://hub.example.com --workspace ~/anycon --name mac-mini --yes
anycon daemon             # 前台运行；或 `anycon service install` 注册为 launchd / systemd 服务
anycon pair               # 另开一个终端：显示 16 位短码和配对链接（--qr 画二维码）
```

每个 [GitHub Release](https://github.com/anycon-ai/anycon/releases) 也附带预构建的二进制、校验和与构建来源证明。想从源码构建：在克隆下来的仓库里跑 `cargo install --path crates/cli --locked`（需要较新的 stable Rust 工具链），hub 镜像用 `docker build -f docker/hub.Dockerfile -t anycon-hub .`。

**3. 浏览器打开 hub 地址**，用管理员 token 登录，输入短码。完成——`~/anycon/` 下每个文件夹都是一个项目，可以在浏览器里直接开始工作。再加一台设备：在节点设置里点「添加设备」，或者再跑一次 `anycon pair`。

## 客户端

**Web UI**：由 hub 提供，可安装为 PWA。流式 Markdown、工具调用渲染、diff、权限审批、模型 / 思考 / 权限模式选择、每会话 worktree、图片附件、Web Push 通知。中英文界面。

**CLI**：任何终端都能用。

```bash
anycon connect https://hub.example.com --code 7K3P-Q2MX-4FWD-H8RT
anycon new --project my-app "跑一下测试，把失败的修了"
```

输出实时流式显示，权限请求直接在终端里答复。

**TypeScript SDK**（[`@anycon/sdk`](packages/sdk)）：

```ts
import { pair, connect } from '@anycon/sdk'

await pair('https://hub.example.com', { code: '7K3P-Q2MX-4FWD-H8RT', name: 'ci-runner' }) // 一次性

const node = await connect('https://hub.example.com', { node: 'mac-mini' })
const session = await node.sessions.create({ project: 'my-app', agent: 'claude-code' })
for await (const ev of session.send('跑一下测试，把失败的修了')) {
  if (ev.type === 'text_delta') process.stdout.write(ev.text)
  if (ev.type === 'permission') await ev.allow()
}
```

## 支持的 agent

| Agent | 方式 |
|---|---|
| Claude Code | 原生 adapter（`stream-json` 双向） |
| Codex | 原生 adapter（`codex app-server`，`codex-cli` ≥ 0.157.0） |
| Mock agent | 确定性的假 agent，开发和测试用（`ANYCON_MOCK_AGENT=1`） |

装了就会自动出现，`anycon config set agents.<id>.enabled false` 关掉。接下来：[ACP](https://agentclientprotocol.com) agent（OpenCode、Pi……），以及用 SDK 把自己的 agent 注册为节点，见[路线图](docs/roadmap.md)。

## 隐私模型

- 会话历史存在**你的机器**（daemon）上，不在 hub
- daemon 和每个客户端之间走 **Noise 端到端加密**。hub 看得到的是元数据：哪个设备连哪个设备、什么时候、多少字节，加上设备名、账号标识这类账号层信息
- **账号信息很少**：显示名、你绑定的邮箱（或 GitHub / Google 声明已验证的邮箱）、关联的 GitHub / Google 账号的提供方和稳定用户 ID（不存用户名、头像和 access token）、设备列表、登录等审计记录。提供方没有声明已验证的邮箱，hub 不会拿来匹配或创建账号。你可以在设置里删除账号：hub 上的这些记录（设备、配对、登录方式、审计）一并删掉，只留一条没有 IP 的删除记录；会话历史本来就在你的节点上，不受影响
- **Web UI 的代码由 hub 下发**。一个恶意或被入侵的 hub 可以下发改过的 JavaScript，读走浏览器里的密钥和明文。所以用 Web UI 就是信任那个 hub 的运营者。隐私敏感的场景请**自建 hub**，或者用 **CLI / SDK**（代码是你自己装的）
- 配对短码在 daemon 本地生成，不发给 hub；两端用 Argon2id 从短码派生预共享密钥。默认的 16 位短码即使面对恶意 hub 也扛得住离线穷举；8 位的 `anycon pair --short` 只在自建 hub 上用
- 浏览器里的密钥和缓存以明文存在 IndexedDB。公用电脑上请用「退出并移除本设备」，不要只点「退出登录」
- **dev server 预览**跑在单独的域名上（公共 hub 是 `*.aiplace.app`），经自己的 Noise 隧道到 daemon。hub 看得到有一个预览开着、流量多大，看不到页面内容。预览的加载代码同样由 hub 下发，信任前提和 Web UI 一样。daemon 默认只转发 agent 会话自己起的服务所监听的端口
- Anycon 不是沙箱（见上面的「代价」）

完整的威胁模型见 [docs/architecture.md § 5.6](docs/architecture.md#56-威胁模型) 和 [SECURITY.md](SECURITY.md)。

## 文档

| | |
|---|---|
| [架构](docs/architecture.md) | 组件、数据归属、加密模型、workspace |
| [协议](docs/protocol.md) | 信封、配对握手、JSON-RPC 方法、离线行为 |
| [功能规格](docs/spec/) | daemon、hub、Web UI、CLI、SDK 各自具体做什么 |
| [部署](docs/deploy.md) | 自建 hub、反向代理、备份、升级 |
| [决策记录](docs/decisions.md) | 架构决策（ADR） |
| [路线图](docs/roadmap.md) | v0.1 有什么、接下来做什么 |

## 开发

工具链版本钉在 [`mise.toml`](mise.toml)（Rust、bun、Node、just）。

```bash
bun install
just gen-wasm && bun run --cwd web build   # 首次：Noise wasm + Web UI（嵌入 hub）
just dev     # hub :8080 + daemon（mock agent）+ Web dev server :5173
just test    # Rust + TypeScript 测试
just lint    # fmt、clippy、oxlint、typecheck
just e2e     # Playwright：真实 hub + daemon + Web UI
```

开发流程、约定和最需要帮忙的地方见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 状态

Anycon 处于 **v0.1**。核心流程——启动 hub、配对、和 agent 对话、审批权限、在 worktree 里工作——端到端跑通且有自动化测试。协议和 API 在 minor 版本之间仍可能变化。问题和不顺手的地方请提 [issue](https://github.com/anycon-ai/anycon/issues)，安全问题按 [SECURITY.md](SECURITY.md) 里的方式报告。

## License

[Apache-2.0](LICENSE)
