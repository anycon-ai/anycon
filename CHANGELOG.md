# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/). While on 0.x, any minor release may contain breaking protocol or API changes.

## [Unreleased]

### Added

- Answered questions stay in the conversation: each question is listed with its answer, and a note sent with a reply (a skip note or a denial reason) is shown under it. Protocol: `permission_resolved` carries the device's `answers` and `message`. Both fields are optional, and replies recorded before this change show the one-line summary as before.
- Claude Code background commands (`run_in_background`) show "running in background" until they finish, then get their final result and exit code. The session panel's Agents card lists them while they run and counts them as working, along with any other background tasks the daemon reports.
- Web UI: add a project without going to the node's home page. Use the new button next to search in the sidebar, "New project…" at the end of the project list on the new-session page, or "New project" / "Clone a repository" / "Open an existing folder" in the ⌘K palette. The dialog can switch nodes, and a project created from the new-session page is selected there.
- Web UI: Node settings → About has "Remove this node", for a machine that was reinstalled or retired. It works while the node is offline: the hub revokes the node and this browser forgets it. Revoking a node under Settings → Devices now forgets it too. Other browsers list a node removed this way under "Removed from your account" and can forget it. Projects and session history on the machine itself are not touched.
- Dev server preview ([ADR-39](docs/decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)): open a dev server running on the daemon machine from the session page, embedded or in a new tab. The page loads on a separate preview domain (`ANYCON_PREVIEW_URL`, `https://*.aiplace.app` on the public hub) and its traffic goes through its own Noise tunnel to the daemon, so the hub still only forwards ciphertext. By default the daemon only forwards to ports opened by an agent session; other ports go in `preview.allow_ports`. Protocol: `preview.*` methods, `auth.preview_token`, `preview_peer`, and an optional `to` on `rate_limited`. Node settings gain a Preview tab that lists and closes previews.
- Hub `multi` mode (`ANYCON_MODE=multi`, [ADR-40](docs/decisions.md#adr-40-multi-模式开放注册管理员配额账号删除和-oauth-绑定)): open registration, the configuration for a public hub. The first email-code or GitHub / Google sign-in creates the account. There is no bootstrap token, token login, or invitation, and the hub refuses to start unless SMTP or an OAuth provider is configured. Admins are the accounts whose email is in `ANYCON_ADMIN_EMAILS`, plus accounts an admin has promoted. An existing `single` database can be switched to `multi`. See [docs/deploy.md § 9](docs/deploy.md#9-公共-hub-与-multi-模式).
- Per-account quotas in `multi` mode: devices, daemons, simultaneous connections, and pairing / enrollment attempts per rolling 24 hours (`ANYCON_QUOTA_DEVICES`, `_DAEMONS`, `_CONNECTIONS`, `_DAILY_PAIRINGS`; defaults 10 / 3 / 10 / 20). Every completed match costs exactly one slot: starting a pairing or enrollment counts once (`pair_begin`, `enroll_begin`), and when the node that started the pairing has no account yet (a first-time daemon), the account that pairs it pays the slot instead. A mistyped code costs nothing, and an account cannot pair fresh daemons all day. A pairing that would go over fails with `pair_failed { quota }` / `enroll_failed { quota }` and creates no device, and a connection over the limit gets `auth_fail { quota }`. Protocol: `auth_ok.quota` and `quota_update` carry the usage. The hub also serves `GET /api/quota`, and the Web UI has Settings → Quota.
- GitHub and Google sign-in (`ANYCON_OAUTH_GITHUB_CLIENT_ID` / `_CLIENT_SECRET`, `ANYCON_OAUTH_GOOGLE_…`), in both modes. Authorization code flow with PKCE and a state cookie; the callback only works on `ANYCON_PUBLIC_URL`, so the reverse proxy must pass the original `Host` (a request on another host is redirected to the public address at most once, marked `hop=1`, so a proxy that rewrites `Host` cannot cause a redirect loop). Failures, including a hub-side error (`server_error`), redirect back to the Web UI with a stable `oauth_error` code. In `single` mode new users sign up through OAuth with an invitation. Link and unlink accounts under Settings → Login methods (`GET /api/me/identities`, `DELETE /api/me/identities/:provider`). An email the provider has not verified is never used to match or create an account. The hub stores only the provider and its stable user ID, not names, emails, or tokens.
- Email-code sign-in in `multi` mode: the hub sends a code to any valid address, and entering it creates the account if there is none yet.
- Delete your account: Settings → Profile (`DELETE /api/me`, needs a sign-in from the last 10 minutes). The hub removes the account's devices, pairings, sign-in methods, and audit records, disconnects it immediately, and keeps a single `account_deleted` record. Session history on your nodes is not touched.
- Admin page: search accounts (substring of the name or email, or an exact ID; ASCII letters are case-insensitive, non-ASCII characters in a display name must match in case), grant or remove admin, and delete accounts (`GET /api/admin/accounts?q=`, `POST` / `DELETE /api/admin/accounts/:id/admin`, `DELETE /api/admin/accounts/:id`). Invitations are hidden in `multi` mode.
- Database: migration `0003_identities.sql` adds the `identities` and `oauth_states` tables. Migrations only move forward, so back up `/data` before upgrading if you may want to roll back.

### Changed

- `login_methods` in `/api/config` and `/api/me` is ordered `github`, `google`, `email`, `token`, so a `single` hub with SMTP now reports `["email","token"]`. The CLI and Web UI do not depend on the order.
- `ANYCON_ADMIN_EMAILS`, `ANYCON_QUOTA_DEVICES`, `_DAEMONS`, `_CONNECTIONS`, `_DAILY_PAIRINGS`, and `ANYCON_OAUTH_*` no longer log "recognized but not implemented". Blank values count as unset. A value that is set but invalid stops the hub from starting, in both modes. In the mode that does not use a variable it is ignored with a warning, and `ANYCON_ADMIN_TOKEN` is ignored in `multi`.
- Removing an account's last way to sign in is refused with `409 last_login_method`: unbinding the email, unlinking a GitHub / Google account, and (in `single`) deleting a login token. The three are serialized per account and judged on the account as it is inside the lock, so two requests at the same time cannot both succeed and leave the account locked out. The `single` bootstrap admin always keeps the bootstrap token.
- Web UI: the message list and the input share one column. Its text width follows the center pane, up to 72rem, with side margins that grow from 16px to 80px as the pane widens. User message bubbles are allowed up to 56rem.
- Web UI: on wide screens, the "will be sent after this turn" notice sits on the same row as the branch chip, on the left.

### Fixed

- Web UI: the sidebar no longer lists a project the node no longer has (moved to trash, unlinked, or its folder deleted). Its sessions are still on the node and open from the node's home page, search, and the project page.
- Web UI: the "Incompatible versions" and "This node no longer recognizes this device" pages have a "Node settings" button and no longer block the node's settings page, so a node that may never connect again can still be removed from the account.
- `anycon pair` on an already paired node stops as soon as the hub refuses the pairing (for example `quota` when the account's device limit is full, or the new device disconnecting) and says why, instead of waiting until the code expires and reporting a timeout. It also stops, saying so, when another pairing was started on the same node (the Web UI's Add device, a second `anycon pair`) and replaced its code. A refusal when the code is registered (the daily pairing limit, rate limiting) is no longer reported as the node being offline: `node.pair_begin` returns `busy` with `data.reason` `quota` / `rate_limited` (or `forbidden`), `unsupported` stays "not connected to the hub", and the Web UI's Add device dialog explains the quota case with a link to Settings → Quota. `--json` prints `{"failed":{"reason":…,"message":…}}` as the last line. The node keeps the outcome of the pairing it started and exposes it through the local-only `local.pair_outcome`.
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
