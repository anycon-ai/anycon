<p align="center">
  <img src="web/public/logo.svg" width="88" alt="">
</p>

<h1 align="center">Anycon</h1>

<p align="center">
  Run coding agents on your own machine. Talk to them from anywhere.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License: Apache-2.0"></a>
  <img src="https://img.shields.io/badge/status-v0.2-orange" alt="Status: v0.2">
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

You need Docker for the hub. The `anycon` binary (daemon and CLI in one) runs on macOS (Apple silicon and Intel), Linux (x86_64 and arm64, glibc 2.35 or newer), and Windows (x64, Windows 10 1809 or newer).

**Don't want to run a hub yet?** Use the public hub at [hub.anycon.ai](https://hub.anycon.ai): sign in with your email address, then skip to step 2 and use `--hub https://hub.anycon.ai`. The free plan has one node (a machine running the daemon) per account. It runs the same release as a self-hosted hub, but its Web UI's code is served by the hub, so using that Web UI means trusting its operator ([privacy model](#privacy-model)). For anything sensitive, run your own hub or use the CLI / SDK. Its [terms](https://anycon.ai/terms) and [privacy policy](https://anycon.ai/privacy) are on anycon.ai.

**1. Run a hub.** It can run on the same machine as the agents, or anywhere both sides can reach.

```bash
docker run -d --name anycon-hub -p 8900:8900 -v anycon-data:/data \
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

On Windows, in PowerShell (installs to `%LOCALAPPDATA%\Programs\anycon\bin` and adds it to your PATH; no administrator rights needed):

```powershell
irm https://github.com/anycon-ai/anycon/releases/latest/download/install.ps1 | iex

anycon init --hub https://hub.example.com --workspace ~/anycon --name my-pc --yes
anycon service install    # a scheduled task that starts the daemon at logon, without a console window
anycon pair
```

Prebuilt binaries are also attached to each [GitHub release](https://github.com/anycon-ai/anycon/releases), with checksums and build provenance. To build from source instead, run `cargo install --path crates/cli --locked` in a clone (needs a current stable Rust toolchain; on Windows also `cargo install --path crates/windows-launcher --locked` for the console-less launcher the service uses), and `docker build -f docker/hub.Dockerfile -t anycon-hub .` for the hub image.

**3. Open the hub in a browser**, sign in with the admin token (on the public hub, with your email), and enter the pairing code. That's it. Every folder under `~/anycon/` is now a project you can work in from the browser. To add another device, use *Add device* in node settings, or run `anycon pair` again.

## Clients

**Web UI.** Served by the hub and installable as a PWA. It has streaming Markdown, tool-call rendering, diffs, permission approvals, model / reasoning / permission-mode pickers, per-session worktrees, image and file attachments, and Web Push notifications. Available in English and Chinese.

**CLI.** Use it from any terminal:

```bash
anycon connect https://hub.example.com --code 7K3P-Q2MX-4FWD-H8RT
anycon new --project my-app "run the tests and fix whatever fails"
```

Output streams live, and you answer permission requests right in the terminal.

**MCP server.** `anycon mcp` runs an [MCP](https://modelcontextprotocol.io) server on stdio, so another agent can list, read, search, and drive the sessions on your node (set it up as an MCP server with command `anycon` and args `["mcp"]`). It has no tool to approve permission requests, which stay with you; it refuses dangerous permission modes unless you start it with `--allow-dangerous-modes`, and refuses to create or drive sessions from inside an anycon session unless you pass `--allow-nested`. See [ADR-49](docs/decisions.md#adr-49-anycon-mcpstdio-mcp-server-是又一个客户端入口不暴露审批不做编排) and the [CLI spec](docs/spec/cli.md#37-anycon-mcp).

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
| OpenCode, Gemini CLI, Goose, Kimi Code, Qwen Code, Cursor, GitHub Copilot, Pi | [ACP](https://agentclientprotocol.com) v1 adapter (one process per agent, shared by its sessions; the command must already be on the daemon's `PATH`, nothing is fetched with `npx` / `uvx`) |
| Mock agent | Deterministic fake agent for development and tests (`ANYCON_MOCK_AGENT=1`) |

Installed agents are picked up automatically. Use `anycon config set agents.<id>.enabled false` to turn one off (`agents.acp.<id>.enabled` for an ACP agent). Any other ACP agent can be added with `anycon config set agents.acp.<name>.command "<command>"`. Planned next: custom agents registered as nodes through the SDK. See the [roadmap](docs/roadmap.md).

## Privacy model

- Session history is stored **on your machine** (the daemon), never on the hub.
- The daemon and every client talk over **Noise end-to-end encryption**. The hub sees metadata: which device talks to which, when, and how many bytes, plus account-level data like device names and account identifiers.
- **Accounts hold little.** The hub stores a display name, an email if you bound one (or one that GitHub / Google reported as verified), the provider and stable user ID of any linked GitHub / Google account (no username, avatar, or access token), your device list (including when each device was last seen and the network prefix of the IP address it last connected from: the first 24 bits of an IPv4 address or 48 bits of an IPv6 address, never the full address), and audit records such as sign-ins. An email the provider has not verified is never used to match or create an account. You can delete your account in settings: the hub removes those records (devices, pairings, sign-in methods, audit rows) and keeps only a deletion record without an IP address. Session history was never on the hub, so it is unaffected.
- **The Web UI's code is served by the hub.** A malicious or compromised hub could serve modified JavaScript and read keys and plaintext in your browser. Using the Web UI therefore means trusting the operator of that hub. For sensitive use, **self-host the hub**, or use the **CLI / SDK**, whose code you install yourself.
- Pairing codes are generated on the daemon and never sent to the hub. Both sides derive the pre-shared key from the code with Argon2id. The default 16-character code holds up against offline guessing even by a malicious hub. Use the 8-character `anycon pair --short` only with a hub you run yourself.
- Browser keys and caches are stored unencrypted in IndexedDB. That includes the titles and notes of your to-do items (the local copy of the list and any changes not yet sent to the node), with the same exposure as the session cache. What clears what: *Sign out* clears nothing on disk. *Sign out and remove this device* deletes the whole local database, including the to-do cache, queued changes and any shared text not yet picked up. Forgetting a node deletes that node's cached to-dos and its queued changes. Signing in as a different account wipes all local data, including a share that was never picked up. *Settings > Storage > Clear the cache* deletes only the cached session history and attachments (attachments that drafts and unsent messages still need stay), and keeps the device key, drafts, unsent messages and the to-do cache, so it is not a substitute for *Sign out and remove this device*. Without clearing, the session history cache holds the 20 sessions opened most recently in that browser, up to 2,000 events each. On a shared computer, use *Sign out and remove this device*, not just *Sign out*.
- **Sharing text into the Web UI.** When the installed web app receives a share from another app, its service worker handles it: the shared text is kept in a small browser database, never put in a URL, and nothing is added to your list until you confirm. When no service worker with that handler is in control (none is active yet; an older version without the handler is still in control, which is the case for every existing user until they accept the update or close every window and tab of the app, since a reload does not activate a waiting service worker; or service workers are unavailable or disabled in the browser), that one request reaches the hub instead. The hub answers with a redirect and discards the body without parsing, storing or logging it, but the shared text did pass through the hub, and through any TLS-terminating proxy in front of it, which can see the body.
- **Dev server previews** run on a separate domain (on the public hub, `*.aiplace.app`) and travel through their own Noise tunnel to the daemon. The hub sees that a preview is open and how much traffic it carries, not the pages. The preview loader is also served by the hub, so the same trust caveat as the Web UI applies. By default the daemon only forwards to ports opened by an agent session.
- **An agent you wire to `anycon mcp` acts with the permissions of your CLI** (by default the local daemon's socket; with `--node` / `--hub`, the CLI device this machine has paired). The MCP server exposes eleven tools that map onto existing requests, and its limits sit on that interface: no tool approves or denies a permission request (a call that reaches one returns `waiting_permission` for a person to handle on a paired device); `create_session` refuses permission modes marked dangerous and `send_message` refuses sessions already in one, unless you start the server with `--allow-dangerous-modes` (the node's `permissions.allow_dangerous_modes` still applies); there is no session `env`; and inside an agent that anycon started, creating, driving and cancelling sessions is refused unless you pass `--allow-nested`. These are guardrails on the interface, not isolation: an agent can change its own environment and MCP configuration, and one that can run shell commands can run `anycon` itself. Actions that come through it are recorded as `local-mcp` in the audit log.
- **Agents can add to your to-do list, and only add.** The MCP tool `add_todo`, and `anycon todo add` run inside an agent that anycon started, put an item on the node's to-do list without `--allow-nested`. Such an item is marked as added by an agent and as unreviewed until you look at it, and the node keeps at most 100 of those. An agent cannot change, close or delete items through these interfaces (the MCP server has no tool for it, and `anycon todo done` / `drop` / `reopen` / `rm` refuse inside an agent session). The agent marking is self-reported, so it is a guardrail: a process that deliberately hides where it comes from, or any shell on a machine whose CLI is paired with the node, can add items that look like yours and can change the list. To-do titles and notes are your content: they are stored in plain text in the daemon's database, like session history, and travel only over the end-to-end channel, so the hub sees only metadata such as message sizes and timing.
- **Handing a to-do to an agent passes earlier agents' output on.** When you dispatch a to-do item (`anycon todo run`, `todo.dispatch`), the node opens a session for it and, at the end of each turn, records the agent's last text (up to 4 KiB) on the item. When you dispatch the same item again, the reports of the last five dispatches (up to 8 KiB in all) are put into the new session's first message, labelled as reports from earlier sessions and not as instructions. A session that was hit by a prompt injection can therefore write text that reaches the next agent's input. The label (and the `> ` prefix on every line of a report) is a hint, not a defence against injection. The node generates that message itself and refuses `todo.dispatch` from a connection that declared itself `mcp`, and `anycon todo run` refuses inside an agent session that anycon started; both are guard rails, not a boundary, so an agent with a shell can still dispatch. The recorded text is session content: it stays out of logs and the audit log, and it travels only over the end-to-end channel (the hub sees only metadata such as message sizes and timing). Dispatches start with the agent's "no push, no pull request" option on where the agent has one (Claude Code does, as permission rules on `git push`, `gh pr create` and `gh pr merge`). That is a command-pattern guard rail, not a boundary: a script that pushes gets through. Agents that do not have the option (Codex, ACP agents) can be dispatched to only if you explicitly give it up.
- **Active rules are instructions in the system prompt of every session a to-do is handed to.** `anycon rule` keeps a list of rules on the node, such as "write the failing test first". The active ones are written into the system prompt of each session opened by `todo.dispatch` (and of manual sessions too if you set `rules.inject_manual_sessions = true`; it is off by default). A rule you write yourself is active at once. Anything not written by a person, such as a rule an agent proposes, is pending and does nothing until you confirm it, so look at it before you do. An agent can only propose: `rule.add` from a connection that declared itself `mcp`, or from `anycon rule add` inside an agent session that anycon started, is stored as pending; the node refuses such a connection's changes and deletions, `anycon rule confirm` and the others refuse inside an agent session, and `anycon mcp` has no tool for rules. These are guard rails, not a boundary: the marker is self-reported, so an agent with a shell can still write a rule that is active at once. The real gates are your confirmation and a size limit (at most 4 KiB of rules in one session; a rule that would go over is refused, nothing is cut). Rules never change permissions: they do not approve or deny requests, change the permission mode, or relax "no push, no pull request". They are text the agent may ignore, not a defence against prompt injection. Agents that cannot take a system prompt (ACP agents) do not get them. The text of a rule is content like a session: it stays out of logs and the audit log, and travels only over the end-to-end channel (the hub sees only metadata).
- **Rule extraction sends session excerpts to the judgement backend you configure, and only if you configure one.** With a judgement backend set up in `config.toml`, the daemon can pick out the decisions you made in a session and ask the backend to turn them into rules. Those decisions are what you said when answering a permission request or a question, when interjecting, or right after cancelling, plus a little of the agent's text around them. With an `api` backend, the daemon itself sends these excerpts and the text of your existing rules to that model service over HTTPS, not through the hub. With an `agent` backend, a one-shot run of your local agent CLI sends them to that agent's provider. Extracted rules stay pending until you confirm them. Automatic extraction is off unless you turn on `rules.extract_auto`.
- **The bot dispatches sessions only in projects you allow, using the agent’s declared unattended mode (or its default when none is declared), always with “no push, no pull request”; it cannot approve permissions.** Without an available triage backend or allowed projects it does not run. Triage sends assigned items, their earlier results, questions and answers, and active rules to the judgement backend: with `api`, the daemon sends them directly over HTTPS to the configured model service, outside the hub; with `agent`, the local agent CLI sends them to its model provider. For actions the daemon can execute, injected content can at most make the bot choose a wrong action for a to-do item; it cannot make it change item text or approve permissions. This validation does not restrict what the backend itself can access (a one-shot Codex invocation can still run read-only commands). “No push” is a command-pattern guardrail, not a sandbox boundary. Question and hand-back pushes add only a category and an opaque to-do destination; their content is retrieved over the end-to-end channel. The PR state of sessions the bot dispatched is read by the daemon with the local `gh`: only the PR number, state, check summary, review count and latest review time enter the triage input, never review or comment bodies, because anyone can comment on a public repository; to act on review feedback the bot dispatches a session whose agent reads it with `gh` itself, under that agent’s own permissions.
- **A bot connection lets one node's bot add to-do items to another node (B) and dispatch them, so B gains one restricted device (kind `twin`).** Only a person can set it up: on B, `node.twin_link_code` issues a 16-character pairing code (valid for 10 minutes, one at a time) that accepts only a bot device; ordinary pairing codes, join codes and the node password never accept the `twin` kind. B's daemon enforces the limits by device kind, so they do not depend on the other side behaving: a bot device can call only a fixed method whitelist and touch only the to-do items it added and the sessions it dispatched (everything else, including other people's items and sessions, is `forbidden`, before `not_found`, so it cannot probe whether they exist); the only notifications pushed to it are those four kinds about its own items and sessions, never session lists, permission requests, to-do lists, enrolment, security events or project and agent changes, and it gets no `notify`; `node.info` leaves out directory paths and the judgement backends. The scope (which projects are open, how many sessions at once, how many dispatches a day, which agent and mode) is stored on B and set by B's owner, and defaults to nothing open; dispatched sessions always get "no push, no pull request" and a worktree. B's `[twin] accept_links = false` (remote changes can only tighten it) disconnects every bot connection at any time. **If A is compromised, or A's judgement is prompt-injected, it can make B's agents work in the projects B opened to that connection**; the limits are B's scope settings, B's agent reviewer and "no push, no pull request" (a command-pattern guardrail, not a sandbox boundary). The titles, bodies and bot notes of A's items are written to B's database, and the results of B's sessions are fetched back to A; over the hub they stay end-to-end encrypted, and the extra metadata the hub sees is that A's bot device is paired with B, plus the size and timing of the traffic between them. The bot device's key exists only inside A's daemon process; there is no proxy for the judgement backend or other local processes on A, and no RPC method that reaches B through A: B is acted on only by A's bot's plans (executed deterministically by A's daemon) and by the daemon's read-only background sync of the items it dispatched, so other local processes on A have no direct way to use B and can only add items to A's own list and hand them to the bot, as a user could (items added by an agent need a person's approval first). When a connection is made or revoked, B sends a security event and a `security` push to all its paired devices and writes an audit entry. Revoke the bot devices of a B before downgrading it to an older version that does not know the `twin` kind.
- **The daemon checks for updates once a day, unless you turn it off.** When the update check is on, the daemon sends one request a day to `https://anycon.ai/releases.json` to see whether a newer release exists, and shows a notice in `anycon doctor` and the Web UI's node settings. It never downloads or installs anything. The request carries only a `User-Agent` such as `anycon/0.3.1 (linux; x86_64)`: no account, device, node or installation identifier, and no session content. anycon.ai's server and the network provider in front of it (currently Cloudflare) see the request's IP address, time and User-Agent (version, OS, architecture). The project keeps these requests in an access log for at most 30 days and uses them to count active daemons by country, version and platform; afterwards only the counts are kept. To have no contact with anycon.ai, set `[update] check = false` in `config.toml` or `ANYCON_UPDATE_CHECK=0` in the daemon's environment. Paired devices cannot turn it back on remotely. The CLI's other commands, `anycon mcp` and the SDKs make no such request.
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
just dev     # hub :8900 + daemon (mock agent) + Web dev server :5173
just test    # Rust + TypeScript tests
just lint    # fmt, clippy, oxlint, typecheck
just e2e     # Playwright against a real hub + daemon + Web UI
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the dev loop, conventions, and where help is most welcome.

## Status

Anycon is at **v0.2**. The core flow works end to end and has automated tests: run a hub, pair, chat with an agent, approve permissions, work in worktrees. The protocol and APIs may still change between minor versions. Please report bugs and rough edges in [issues](https://github.com/anycon-ai/anycon/issues), and send security problems to the channel in [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE)
