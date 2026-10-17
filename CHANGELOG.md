# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/). While on 0.x, any minor release may contain breaking protocol or API changes.

## [0.1.0] - Unreleased

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

[0.1.0]: https://github.com/anycon-ai/anycon/releases/tag/v0.1.0
