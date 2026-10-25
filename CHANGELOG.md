# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/). While on 0.x, any minor release may contain breaking protocol or API changes.

## [Unreleased]

### Added

- Answered questions stay in the conversation: each question is listed with its answer, and a note sent with a reply (a skip note or a denial reason) is shown under it. Protocol: `permission_resolved` carries the device's `answers` and `message`. Both fields are optional, and replies recorded before this change show the one-line summary as before.
- Claude Code background commands (`run_in_background`) show "running in background" until they finish, then get their final result and exit code. The session panel's Agents card lists them while they run and counts them as working, along with any other background tasks the daemon reports.
- Dev server preview ([ADR-39](docs/decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)): open a dev server running on the daemon machine from the session page, embedded or in a new tab. The page loads on a separate preview domain (`ANYCON_PREVIEW_URL`, `https://*.aiplace.app` on the public hub) and its traffic goes through its own Noise tunnel to the daemon, so the hub still only forwards ciphertext. By default the daemon only forwards to ports opened by an agent session; other ports go in `preview.allow_ports`. Protocol: `preview.*` methods, `auth.preview_token`, `preview_peer`, and an optional `to` on `rate_limited`. Node settings gain a Preview tab that lists and closes previews.

### Changed

- Web UI: the message list and the input share one column. Its text width follows the center pane, up to 72rem, with side margins that grow from 16px to 120px as the pane widens. User message bubbles are allowed up to 56rem.
- Web UI: on wide screens, the "will be sent after this turn" notice sits on the same row as the branch chip, on the left.

### Fixed

- Web UI: the session info drawer no longer pops up by itself when the window is widened past 1200px and narrowed again.
- The daemon reconnects when its hub connection goes silent for 75 seconds. Before, a proxy or TUN that dropped the connection without closing it (for example Clash reloading its config) left the daemon thinking it was online while clients showed the node as offline, until the daemon was restarted.

## [0.1.1] - 2026-10-22

### Added

- Subagent tool calls are grouped under the subagent that made them, with its description, call count, state, and result. Claude Code only for now.
- The session panel shows an Agents card: the main agent and every subagent in the loaded history, with state, tokens, and tool-call count. It replaces the Options card (the same controls sit under the input) and the Files changed card (the Changes view covers it).
- Protocol: `tool_call.parent_id`, `tool_result.detail.background`, `tool_result.detail.total_tokens`, and `background_tasks` on `SessionSummary` / `SessionChanged`. All are optional, so older clients and daemons keep working.
- Web UI: drag the desktop sidebar's right edge to resize it (double-click resets it).
- Web UI: a "Block page zoom" switch under Settings → Appearance on iOS and Android. It is off by default.

### Changed

- The default `sessions.idle_timeout` is now 20m (was 10m). A `config.toml` written by 0.1.0 has `idle_timeout = "10m"` in it and keeps that value until you edit it.
- A turn the agent starts by itself (Claude Code processing finished background tasks) arrives as `turn_start` with an empty `message_id`. The Web UI marks it with a divider line.
- Web UI: on wide screens the branch chip sits above the input, on the right.
- Web UI: an option changed while a turn is running is shown in a muted color until the turn ends, instead of a hint line that shifted the layout.
- Faster connect: the Web UI sends its initial sync requests concurrently, and the hub and daemon record last-seen times off the handshake path.

### Fixed

- Claude Code background subagents and commands are no longer killed when `idle_timeout` reclaims the agent process. Reclaim, `session_idle`, and the `turn_finished` notification wait until they finish.
- Re-pairing a revoked device no longer fails with "the code is wrong or has expired". The browser no longer sends the old node link's IK into the new pairing handshake.
- Pairing no longer stalls when the hub bounces a handshake envelope under backpressure. The daemon now resends it.
- Codex: the model set in `~/.codex/config.toml` shows up as an option even when `model/list` omits it (common with a custom `model_provider`).
- Question cards show a single submit button, and it sends the collected answers.
- The SDK no longer matches an agent-started turn to a `send()`.

## [0.1.0] - 2026-10-18

First public release.

### Hub (`anycon-hub`)

- Single-tenant (`single`) mode with admin-token login and invite links. It runs as one container and one SQLite file, with an embedded Web UI.
- Device authentication, routing, and pairing / enrollment matchmaking. The hub only relays end-to-end encrypted envelopes and sees metadata.
- Web Push, email-code login for accounts with a bound email, and SMTP email notifications.
- `/healthz`, `/readyz`, strict CSP, and rate limiting. Ready-made reverse-proxy configs for Caddy, nginx, Traefik, and Cloudflare Tunnel.

### Daemon and CLI (`anycon`)

- Outbound connection to the hub, with reconnect, sleep recovery, and launchd / systemd user services.
- Noise XX+psk3 pairing with locally generated codes (Argon2id-stretched) and Noise IK reconnects.
- Account enrollment: node password, QR approval, and one-time enrollment codes (CPace).
- Claude Code and Codex adapters, agent detection, and per-session model / reasoning / permission-mode settings.
- Permission requests answered from any paired client, with a local audit log.
- Workspace: every folder is a project. Create, clone, rename, trash and restore projects, or link existing directories.
- Per-session git worktrees: diff, sync with base, archive / delete handling, PR status, and auto-archive.
- Image attachments.
- CLI clients: `connect`, `new`, `send`, `attach`, `approve`, `diff`, `sync`, `worktrees`, and more.

### Web UI

- Streaming chat with tool-call rendering, thinking blocks, diffs, permission cards, and image attachments.
- Worktree workflow: changes tab, branch chip, base sync, and PR status.
- Node and account settings, notifications, IndexedDB cache, installable PWA, English and Chinese.

### SDK

- `@anycon/protocol` (generated types, envelope and JSON-RPC codecs, Noise wasm) and `@anycon/sdk` (client direction, image upload).

### Distribution

- Hub image `ghcr.io/anycon-ai/hub` for linux/amd64 and linux/arm64.
- `anycon` binaries for macOS (arm64, x64) and Linux (x64, arm64, glibc 2.35+), installable with `brew install anycon-ai/tap/anycon` or the release's `install.sh`, with SHA-256 checksums and build provenance.
- `@anycon/protocol` and `@anycon/sdk` on npm, published with provenance.
- Every artifact ships its third-party license notices.

[0.1.1]: https://github.com/anycon-ai/anycon/releases/tag/v0.1.1
[0.1.0]: https://github.com/anycon-ai/anycon/releases/tag/v0.1.0
