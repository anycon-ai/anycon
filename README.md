<p align="center">
  <img src="web/public/logo.svg" width="88" alt="">
</p>

<h1 align="center">Anycon</h1>

<p align="center">
  Run coding agents on your own machine. Talk to them from anywhere.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License: Apache-2.0"></a>
  <img src="https://img.shields.io/badge/status-v0.1-orange" alt="Status: v0.1">
  <a href="README.zh-CN.md">简体中文</a>
</p>

---

Anycon connects the machine that runs your AI agents (a Mac mini at home, a workstation, a server) to a small relay called the **hub**, so you can drive those agents from a browser, your phone, a terminal, or a script, from anywhere. The machine dials out to the hub. You don't need a public IP, a VPN, or port forwarding.

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

## Why

Vendor cloud agents run in the vendor's sandbox: your repo is cloned into a container, dependencies are reinstalled, and your internal services are out of reach. Anycon keeps the agent on **your** machine, with your local databases, GPUs, credentials, and tools already in place, and gives you a remote line to it.

- **Self-host first.** The hub is one container and one SQLite file, with no external services.
- **The hub only sees metadata.** Session history lives on your machine. Clients and the daemon talk over end-to-end encrypted Noise sessions, and the hub only routes ciphertext and sees metadata. (Read the [privacy model](#privacy-model) for the one big caveat.)
- **Agent-agnostic.** Claude Code and Codex work out of the box. More agents can be added through the adapter interface.
- **Folders are projects.** Every folder in the workspace directory is a project. Create, clone, or link existing directories from any client.
- **A real coding workflow.** Each session can get its own git worktree and branch. You can review diffs, sync with the base branch, approve permission requests, and let the agent commit and open PRs. All of it runs on your machine.
- **Protocol first.** The Web UI, CLI, and SDK all use the same public protocol.

The trade-offs, stated plainly:

- **No isolation.** Agents run as your user on your machine. Workspace boundaries rely on the agent's own permission prompts, not an OS sandbox.
- **Your machine has to stay on**, and keeping it online and backed up is up to you.
- **The model API still sees your context.** The Claude Code on your machine still sends prompts to its provider. Anycon doesn't change that.

**Not in scope:** terminals and remote shells, editors, issue tracking, team collaboration. Dev server previews are tunneled end to end, never proxied through the hub in plaintext (see the privacy model).

## Quick start

You need Docker for the hub. The `anycon` binary (daemon and CLI in one) runs on macOS (Apple silicon and Intel) and Linux (x86_64 and arm64, glibc 2.35 or newer).

**1. Run a hub.** It can run on the same machine as the agents, or anywhere both sides can reach.

```bash
docker run -d --name anycon-hub -p 8080:8080 -v anycon-data:/data \
  -e ANYCON_PUBLIC_URL=https://hub.example.com ghcr.io/anycon-ai/hub:latest
docker logs anycon-hub    # prints a one-time admin token on first start
```

Want people to sign up on their own (email code or GitHub / Google sign-in, with per-account quotas)? Run it with `ANYCON_MODE=multi`; see [docs/deploy.md § 9](docs/deploy.md#9-公共-hub-与-multi-模式).

The image is built for `linux/amd64` and `linux/arm64` and tagged `<version>` (e.g. `0.1.0`), `<major>.<minor>` (`0.1`), and `latest`. Pin a version in production. The hub serves plain HTTP. TLS and your domain are handled by your reverse proxy. [docs/deploy.md](docs/deploy.md) has ready-to-use configs for Caddy, nginx, Traefik, and Cloudflare Tunnel, plus a [compose file](docker/compose.yml).

**2. Install and pair the daemon** on the machine that runs your agents:

```bash
brew install anycon-ai/tap/anycon
# or, without Homebrew (installs to ~/.local/bin):
curl -fsSL https://github.com/anycon-ai/anycon/releases/latest/download/install.sh | sh

anycon init --hub https://hub.example.com --workspace ~/anycon --name mac-mini --yes
anycon daemon             # foreground; or `anycon service install` for launchd / systemd
anycon pair               # in another terminal: prints a 16-character code and a pairing link (--qr for a QR code)
```

Prebuilt binaries are also attached to each [GitHub release](https://github.com/anycon-ai/anycon/releases), with checksums and build provenance. To build from source instead, run `cargo install --path crates/cli --locked` in a clone (needs a current stable Rust toolchain), and `docker build -f docker/hub.Dockerfile -t anycon-hub .` for the hub image.

**3. Open the hub in a browser**, sign in with the admin token, and enter the pairing code. That's it. Every folder under `~/anycon/` is now a project you can work in from the browser. To add another device, use *Add device* in node settings, or run `anycon pair` again.

## Clients

**Web UI.** Served by the hub and installable as a PWA. It has streaming Markdown, tool-call rendering, diffs, permission approvals, model / reasoning / permission-mode pickers, per-session worktrees, image attachments, and Web Push notifications. Available in English and Chinese.

**CLI.** Use it from any terminal:

```bash
anycon connect https://hub.example.com --code 7K3P-Q2MX-4FWD-H8RT
anycon new --project my-app "run the tests and fix whatever fails"
```

Output streams live, and you answer permission requests right in the terminal.

**TypeScript SDK** ([`@anycon/sdk`](packages/sdk)):

```ts
import { pair, connect } from '@anycon/sdk'

await pair('https://hub.example.com', { code: '7K3P-Q2MX-4FWD-H8RT', name: 'ci-runner' }) // once

const node = await connect('https://hub.example.com', { node: 'mac-mini' })
const session = await node.sessions.create({ project: 'my-app', agent: 'claude-code' })
for await (const ev of session.send('run the tests and fix whatever fails')) {
  if (ev.type === 'text_delta') process.stdout.write(ev.text)
  if (ev.type === 'permission') await ev.allow()
}
```

## Supported agents

| Agent | How |
|---|---|
| Claude Code | Native adapter (bidirectional `stream-json`) |
| Codex | Native adapter (`codex app-server`, `codex-cli` ≥ 0.157.0) |
| Mock agent | Deterministic fake agent for development and tests (`ANYCON_MOCK_AGENT=1`) |

Installed agents are picked up automatically. Use `anycon config set agents.<id>.enabled false` to turn one off. Planned next: [ACP](https://agentclientprotocol.com) agents (OpenCode, Pi, …) and custom agents registered as nodes through the SDK. See the [roadmap](docs/roadmap.md).

## Privacy model

- Session history is stored **on your machine** (the daemon), never on the hub.
- The daemon and every client talk over **Noise end-to-end encryption**. The hub sees metadata: which device talks to which, when, and how many bytes, plus account-level data like device names and account identifiers.
- **Accounts hold little.** The hub stores a display name, an email if you bound one (or one that GitHub / Google reported as verified), the provider and stable user ID of any linked GitHub / Google account (no username, avatar, or access token), your device list, and audit records such as sign-ins. An email the provider has not verified is never used to match or create an account. You can delete your account in settings: the hub removes those records (devices, pairings, sign-in methods, audit rows) and keeps only a deletion record without an IP address. Session history was never on the hub, so it is unaffected.
- **The Web UI's code is served by the hub.** A malicious or compromised hub could serve modified JavaScript and read keys and plaintext in your browser. Using the Web UI therefore means trusting the operator of that hub. For sensitive use, **self-host the hub**, or use the **CLI / SDK**, whose code you install yourself.
- Pairing codes are generated on the daemon and never sent to the hub. Both sides derive the pre-shared key from the code with Argon2id. The default 16-character code holds up against offline guessing even by a malicious hub. Use the 8-character `anycon pair --short` only with a hub you run yourself.
- Browser keys and caches are stored unencrypted in IndexedDB. On a shared computer, use *Sign out and remove this device*, not just *Sign out*.
- **Dev server previews** run on a separate domain (on the public hub, `*.aiplace.app`) and travel through their own Noise tunnel to the daemon. The hub sees that a preview is open and how much traffic it carries, not the pages. The preview loader is also served by the hub, so the same trust caveat as the Web UI applies. By default the daemon only forwards to ports opened by an agent session.
- Anycon is not a sandbox (see the trade-offs above).

The full threat model is in [docs/architecture.md § 5.6](docs/architecture.md#56-威胁模型) and [SECURITY.md](SECURITY.md).

## Documentation

| | |
|---|---|
| [Architecture](docs/architecture.md) | Components, data ownership, encryption, workspaces |
| [Protocol](docs/protocol.md) | Envelopes, pairing handshake, JSON-RPC methods, offline behavior |
| [Specs](docs/spec/) | What the daemon, hub, Web UI, CLI, and SDK do in detail |
| [Deployment](docs/deploy.md) | Self-hosting, reverse proxies, backups, upgrades |
| [Decisions](docs/decisions.md) | Architecture decision records |
| [Roadmap](docs/roadmap.md) | What's in v0.1 and what's next |

The design docs are in Chinese for now. English versions will follow once v0.1 settles.

## Development

Toolchain versions are pinned in [`mise.toml`](mise.toml) (Rust, bun, Node, just).

```bash
bun install
just gen-wasm && bun run --cwd web build   # first time: Noise wasm + Web UI (embedded into the hub)
just dev     # hub :8080 + daemon (mock agent) + Web dev server :5173
just test    # Rust + TypeScript tests
just lint    # fmt, clippy, oxlint, typecheck
just e2e     # Playwright against a real hub + daemon + Web UI
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the dev loop, conventions, and where help is most welcome.

## Status

Anycon is at **v0.1**. The core flow works end to end and has automated tests: run a hub, pair, chat with an agent, approve permissions, work in worktrees. The protocol and APIs may still change between minor versions. Please report bugs and rough edges in [issues](https://github.com/anycon-ai/anycon/issues), and send security problems to the channel in [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE)
