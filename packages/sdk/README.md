# @anycon/sdk

[简体中文](https://github.com/anycon-ai/anycon/blob/main/packages/sdk/README.zh-CN.md)

The TypeScript SDK for [Anycon](https://github.com/anycon-ai/anycon), **client side**: pair once, then connect to a node (daemon), manage projects and sessions, stream events, and handle permission requests.
The same code runs on Node ≥ 20 and in browsers (Node uses `ws`, browsers use the native WebSocket). ESM only. The full spec is [docs/spec/sdk.md](https://github.com/anycon-ai/anycon/blob/main/docs/spec/sdk.md) (the design docs are in Chinese for now).

```bash
npm install @anycon/sdk
```

```ts
import { pair, connect, policies } from '@anycon/sdk'

const hub = 'https://hub.example.com'

// Once: run `anycon pair` on the node to get a pairing code (16 characters by default; `--short`
// gives 8, which is only suitable for a self-hosted hub). You can also pass the pairing link.
await pair(hub, { code: '7K3P-Q2MX-W4NH-8CRD', name: 'ci-runner' })
// await pair(hub, { code: 'https://hub.example.com/pair#code=7K3P-Q2MX-W4NH-8CRD', name: 'ci-runner' })

const node = await connect(hub, { node: 'mac-mini' }) // `node` can be left out when only one node is paired
const agents = await node.agents.list() // the valid model / thinking / permission-mode values come from here
const session = await node.sessions.create({
  project: 'app',
  agent: 'claude-code',
  settings: { model: 'opus', thinking: 'high', mode: 'acceptEdits' },
})

for await (const ev of session.send('run the tests and fix whatever fails')) {
  if (ev.type === 'text_delta') process.stdout.write(ev.text)
  if (ev.type === 'permission') await ev.allow() // or ev.deny("don't delete files") / ev.allowSession()
  if (ev.type === 'turn_done') console.log(ev.usage)
}

// Rather not answer permission requests by hand? Pass a policy (it applies to sessions the SDK itself is using)
const node2 = await connect(hub, { onPermission: policies.allowReadsDenyRest })
const result = await session.run('translate the README into English') // { text, turnId, reason, usage, events }

await session.setSettings({ model: 'sonnet' }) // applies from the next turn; fields left out stay as they are
await session.setSettings({}, { clear: ['thinking', 'max_turns'] }) // resetting a field to unset needs clear
await session.cancel()
const { events } = await session.history({ afterSeq: 0 })
await node.close()
```

## Per-session worktrees and git (ADR-34)

A session can work in a git worktree (and branch) that the daemon creates for it, leaving the project's main directory alone. `sessions.create` without `workspace` works in the project directory itself. The SDK **does not read the node's `worktree.default` for you**. If you want a worktree, pass one explicitly:

```ts
const branches = await node.projects.branches('app') // { current, default_base, local, remote, truncated? }
const s = await node.sessions.create({
  project: 'app',
  agent: 'claude-code',
  // also accepts branch: 'feature/login'; if left out, the daemon picks a name
  workspace: { mode: 'worktree', base: branches.default_base },
  // optional: send the first message right after creating (the SDK generates a uuid v4 message_id)
  prompt: 'fix the login redirect',
})
// { id, branch, base, base_sha, path, state }: state starts as 'creating', becomes 'active'
// once the worktree is ready, and follows session.changed
s.worktree
// watch the first turn from the start (including the { type: 'git', action: 'worktree_created', … } event)
for await (const ev of s.events({ fromSeq: 0 })) {
  if (ev.type === 'git') console.log(ev.action, ev.branch)
  if (ev.type === 'turn_done') break
}

// branch / HEAD / ahead / behind / uncommitted / conflicts / merging / upstream; with includePr, also pr or pr_error
const st = await s.gitStatus({ includePr: true })
// { from_sha, files: [{ path, status, additions, deletions, untracked? }], truncated? }
const list = await s.diff()
// against: 'base' (the default, worktree sessions only) | 'uncommitted'; the patch is included only with path
const one = await s.diff({ against: 'uncommitted', path: 'src/a.ts' })
// { outcome: 'up_to_date' | 'merged' | 'conflicts' | 'aborted', commits, conflicts? }
const sync = await s.syncBase()
if (sync.outcome === 'conflicts') await s.syncBase({ abort: true })

// 'auto' (the default: remove if clean, keep if there are uncommitted changes) | 'keep' | 'remove' (discards uncommitted changes)
await s.archive({ worktree: 'auto' })
// deleteBranch: also runs git branch -D (unmerged commits are lost); only takes effect when the worktree is removed too
await s.delete({ worktree: 'remove', deleteBranch: true })

// WorktreeInfo[], including kept worktrees and those whose session was deleted
await node.worktrees.list({ project: 'app' })
// session not archived → busy; uncommitted changes without force → invalid (error.reason === 'uncommitted_changes')
await node.worktrees.remove(id, { force: true, deleteBranch: false })
```

- On an archived session, `send()` / `syncBase()` throw `RpcError` (`invalid`, `reason === 'archived'`). Call `unarchive()` first. While a session is running or waiting for approval, `archive()` throws `busy` (`cancel()` first, or answer the permission request).
- `archive()` / `delete()` without arguments send only `session_id`. Sessions without a worktree ignore these options.
- With `prompt`, `create()` doesn't collect the first turn's events for you. Call `s.events({ fromSeq: 0 })` right after creating the session (live events that arrive while history is being backfilled are held and appended afterwards, so seqs are strictly increasing with no duplicates). Permission policies also only answer once the session has an active `events()` / `send()`. You can pass your own idempotency key as `messageId`: if `create()` times out and you retry with the same key, the daemon returns the session it created the first time instead of opening a second one.
- When creation is refused, check `RpcError.code` / `.reason`: the project isn't `worktree_ready` → `invalid` / `not_worktree_ready`; the branch already exists → `exists`; `base` can't be resolved → `invalid` / `bad_base`. `syncBase` failures: a turn is running → `busy`; there are uncommitted changes → `invalid` / `uncommitted_changes`; the previous merge stopped in conflicts → `invalid` / `merge_in_progress`; fetch / merge failed → `internal` / `fetch_failed` / `git_failed`. `gitStatus` on a session that isn't in a git repository → `invalid` / `not_git`.

## Attachments (ADR-35)

Images and other files are first sent to the node over the blob channel (`blob.put_begin` → `blob.put_chunk` in 512 KiB chunks with at most 2 in flight → `blob.put_end`). The message carries only the reference the daemon returns (`AttachmentRef`):

```ts
import { readFile } from 'node:fs/promises'
import { AttachmentError, acceptsAttachment } from '@anycon/sdk'

const ref = await node.uploadImage(await readFile('shot.png'), {
  name: 'shot.png', // without mime, the type is detected from the file header (png / jpeg / gif / webp)
  onProgress: (sent, total) => console.log(`${sent}/${total}`),
}) // → { blob_id, name, mime, bytes, width?, height? }
try {
  // the text can be an empty string
  for await (const ev of session.send("what's in this image?", { attachments: [ref] })) {
    if (ev.type === 'user_message' && ev.attachments) {
      // images from history (fetched page by page with blob.get)
      const { bytes } = await node.downloadAttachment(ev.attachments[0]!)
    }
  }
} catch (err) {
  if (err instanceof AttachmentError && err.reason === 'blob') {
    /* the image was reclaimed: uploadImage again, then resend */
  } else throw err
}
// a first message with only images works too
await node.sessions.create({ project: 'app', agent: 'claude-code', attachments: [ref2] })
// uploaded but not sent; if you don't discard it, the daemon reclaims it after 24 hours
await node.discardAttachment(ref3)

// any other file: same channel, same queue
const doc = await node.uploadFile(await readFile('spec.pdf'), {
  name: 'spec.pdf', // without mime: image header, else the extension, else application/octet-stream
}) // → { blob_id, name, mime: 'application/pdf', bytes }
if (acceptsAttachment(agent, doc.mime)) {
  // agent: an entry of node.agents.list()
  for await (const ev of session.send('summarize this', { attachments: [doc] })) { /* … */ }
}
```

- **Images vs. other files.** `image/png`, `image/jpeg`, `image/gif` and `image/webp` go to the model as images (the daemon checks the file header and reads the size). Any other file is handed to the agent as a path on the daemon's machine; the agent reads it with its own tools (Claude Code may ask for permission to read it). An agent that accepts arbitrary files lists `'*/*'` (`ATTACHMENT_ANY_FILE`) in `capabilities.attachments`; `acceptsAttachment(agent, mime)` checks both named types and the wildcard.
- **`uploadFile(bytes, { name, mime? })`.** Without `mime`, the SDK uses the file header for the four image types, then a small table of common extensions (`pdf`, `txt`, `md`, `csv`, `json`, `html`, `xml`, `yaml`, `zip`, `gz`, `tar`, `js`, `ts`, `py`, `rs`, `go`, …; see `guessMime`), and falls back to `application/octet-stream`. A file whose name ends in `.png` but whose content isn't a PNG is not guessed as an image. A `mime` you pass must be a well-formed `type/subtype` without parameters; it is lowercased before sending, and a malformed one throws `AnyconError('invalid_mime')` with nothing sent. The daemon is the real validation point. `uploadImage` stays image-only: any other `mime`, or no `mime` and an unrecognized header, throws `AnyconError('unsupported_image')`.
- **The SDK doesn't resize or re-encode images.** Keep each image to **≤ 2000 px on the long edge and about ≤ 1.5 MiB** yourself (for example, shrink it with `sharp` / ImageMagick first). The daemon's hard limits are 10 MiB per file (`sessions.max_attachment_bytes`) and at most 20 attachments per message, ≤ 20 MiB in total. Once an image is in a Claude Code session's history, it is resent with every later turn, so large images quickly push the session past Claude's request size limit. In the same way, `node.uploadImage()` uploads the bytes as they are: it **doesn't strip EXIF metadata (including GPS location) and doesn't apply the EXIF orientation**. Only the Web UI re-encodes images before upload. If privacy or orientation matters to you, handle it before `uploadImage` (for example, `sharp(bytes).rotate().toBuffer()` rotates according to EXIF, and its output has no metadata by default).
- Whether an agent accepts a file type: `acceptsAttachment((await node.agents.list())[i], ref.mime)`. Whether the node supports the blob channel: `(await node.info()).capabilities.blobs`.
- Uploads on one `Node` (images and files alike) are **queued and go one at a time** (even a `Promise.all` batch has only one upload in progress, with 2 chunks in flight), so requests like "allow" or "stop" don't wait behind a multi-megabyte file. A chunk that times out (90 seconds per chunk by default, `requestTimeoutMs`) or is lost to a disconnect is retried up to 2 times. Explicit refusals from the daemon are not retried. On failure or `signal` abort, the SDK makes a best-effort `blob.discard`.
- Refusals throw `AttachmentError` (a subclass of `RpcError`; `.code` is unchanged and `.reason` is narrowed):

  | `reason` | `code` | When it happens / what to do |
  |---|---|---|
  | `too_large` | `invalid` | One file is over the node's limit, or one message totals more than 20 MiB: shrink it or split it across messages |
  | `quota` | `busy` | Unreferenced uploads on the node total more than 1 GiB: send them first, or `discardAttachment()` |
  | `disk_full` | `busy` | The node's data disk is nearly full |
  | `blob` | `not_found` | The referenced attachment was reclaimed (unused for 24 hours) or discarded: upload it again |
  | `blob_in_use` | `invalid` | The attachment is already used in another session (an attachment belongs to one session only): upload it again |
  | `in_use` | `invalid` | `discardAttachment` on an attachment that a message already references (it is deleted along with the session) |
  | `finalizing` / `incomplete` / `hash_mismatch` / `mime_mismatch` | `invalid` | The upload failed finalization or verification (the file header doesn't match `mime`, or the content was corrupted in transit) |

  Sending an attachment to an agent that doesn't accept its type → `RpcError` (`unsupported`). More than 8 concurrent uploads from this device → `RpcError` (`busy`, no `reason`). `uploadImage` with a non-image `mime`, or with no `mime` and an unrecognized file header → `AnyconError('unsupported_image')`; `uploadFile` with a malformed `mime` → `AnyconError('invalid_mime')`. In both cases nothing is sent.

## API overview

- `pair(hubUrl, { code, name, storage?, profile?, headers?, ackTimeoutMs? })`: the joiner. `code` can be a 16-character (default) or 8-character (`anycon pair --short`) pairing code (case and hyphens don't matter), or a pairing link `https://hub/pair#code=…` / just `#code=…` (the code is in the URL fragment, which is not sent to the hub). The flow:
  1. Run `derivePairSecrets(code)` locally, **once** (Argon2id, 64 MiB × 3 passes; about 0.1–0.3 s in Node and 0.1–1 s with browser wasm, blocking the thread). `match_id` and the PSK come out of the same computation, and the PSK is zeroed after use. This adds that much time to every `pair()` in a pairing test, so don't set timeouts below 1 s.
  2. Connect to the hub → `PairJoin { match_id }` (**the pairing code is never sent to the hub**) → Noise XXpsk3 (as initiator, with the PSK from step 1) → `PairConfirm` → `PairDone`.
  3. **Wait for the daemon to send `pair.ack` over the newly established Noise transport** (up to `ackTimeoutMs`, 30 seconds by default). Check that `peer_id` is this device's permanent id, that `node_id == PairDone.peer`, and that `fingerprint` matches the fingerprint of the node public key from the handshake. **Only when every check passes is anything saved** (device identity + node public key). A timeout or mismatch → `PairingFailed`, and nothing is kept.

  The node name comes from `pair.ack` (authenticated, with control characters removed). The payload of the third handshake message carries this device's `{ name, kind, platform, version }`, which the daemon uses to display the device. Nodes are keyed by `PairDone.peer` (the permanent device id). If a node with the same id but a different public key is already stored → `NodeKeyChanged` is thrown and nothing is overwritten (see `forgetNode`). If the hub has invalidated the local identity (`auth_fail invalid | revoked` / close 4004), `pair()` clears it and retries with a fresh identity. For `headers`, see [Account ownership](#account-ownership-the-first-pairing-needs-a-cookie) below.
- `forgetNode(hubUrl, nodeNameOrIdPrefix, { storage?, profile? })`: forgets a locally stored node. Use it after `NodeKeyChanged`, once you've confirmed the new public key can be trusted, then `pair()` again.
- `connect(hubUrl, { node?, storage?, onPermission?, headers?, … })` → `Node`. Every time the WebSocket connects, or `peer_online` arrives (or `auth_ok.peers_online` includes the node), it does a new IK handshake. After a disconnect it reconnects with exponential backoff (1 s → 30 s; a hub restart, close code `4002`, uses 1–3 s with jitter).
- Several nodes at once: `const client = await Client.open(hubUrl)` plus `client.node('name')` for each node (the hub allows only one connection per device, so the nodes share one `Client`).
- `Node`: `info()`, `agents.list({ refresh? })` (cached by etag), `projects.list/create/clone/link/unlink/browse/branches/rename/trash/trashList/restore/purge`, `sessions.list/get/create/search` (`create` accepts `workspace`, `prompt`, `attachments`, `messageId`), `worktrees.list/remove`, `uploadImage(bytes, { name, mime?, signal?, onProgress?, requestTimeoutMs? })` / `uploadFile(bytes, { name, mime?, signal?, onProgress?, requestTimeoutMs? })` / `downloadAttachment(refOrBlobId)` / `discardAttachment(refOrBlobId)` (see [Attachments](#attachments-adr-35) above), `permissions.list`, `online` / `waitOnline()`, the events `online` / `offline` / `permission` / `permission_error` / `error` (non-fatal channel problems) / `session_changed` …, `call(method, params)` (typed), and `rpc(method, params)` (raw).
  - `projects.link(path, { name? })` registers an existing directory on the daemon's machine as a project, without moving or copying it. The path must be inside the daemon's `workspace.link_roots`. `unlink(name)` only removes the link and leaves the directory alone. `browse({ path?, showHidden? })` lists the subdirectories inside `link_roots` to choose from. Linked projects have `kind` `linked`, and `missing` is true when the directory is gone.
- `Session`: `send(text, { attachments?, messageId?, signal? })` → an iterable event stream (**sent immediately**; iteration runs until this turn's `turn_done` or an unrecoverable `error`; a `break` midway doesn't cancel the turn), `run(text)`, `respond(requestId, optionId | { answers }, message?)`, `setSettings(settings, { clear? })`, `cancel`, `history`, `events()`, `rename/archive/unarchive/delete/restart` (`archive({ worktree? })`, `delete({ worktree?, deleteBranch? })`), `worktree` (getter), `gitStatus({ includePr? })`, `diff({ against?, path? })`, `syncBase({ abort? })`. `watch` / `unwatch` and catching up on events after a disconnect (`session.history`) are handled internally.
- Permissions: the `permission` event has `allow()` / `allowSession()` / `deny(message?)` / `respond(optionId)` / `answer(answers)`. An `onPermission` policy returns `'allow' | 'allow_session' | 'deny' | { option, message? } | { deny } | { answers } | undefined`. **Policies answer only for sessions the SDK itself is using (with an active `send()` / `events()`)**, and never decide for sessions the user opened in the Web UI. The SDK denies requests that nobody answers after `permissionTimeoutMs` (10 minutes by default).
  - **No silent escalation to `allow_always`.** `allow()` / `'allow'` only pick `allow_once`. If the request has no `allow_once` option, `allow()` fails with `no_matching_option`, and a policy's `'allow'` emits `node.on('permission_error', (error, request) => …)` and falls through to the timeout deny. To allow for the whole session, use `allowSession()` / `'allow_session'` explicitly (when there is no `allow_always` option it falls back to `allow_once`, which can only narrow the grant).
  - `policies.allowAll` **doesn't approve `mode_change` or `plan`** (switching the permission mode or approving a plan is an escalation), and doesn't handle `question`. Those are left to the caller or the timeout deny. To approve them as well, opt in explicitly: `policies.allowAllWith({ modeChange: true, plan: true })`. A policy callback that throws also emits `permission_error` instead of being swallowed.
- Errors: `PairingRequired`, `PairingFailed(reason)`, `NodeKeyChanged`, `NodeOffline`, `NodeRejected`, `AgentUnavailable(reason)`, `SettingsInvalid(field, reason)`, `NodeBusy`, `ProtocolMismatch(ours, theirs)`, `AttachmentError(code, reason)` (attachment refused; a subclass of `RpcError`), `RpcError(code)` (other protocol errors; `.code` is a stable snake_case string such as `already_resolved`), `ConnectionError`, `AnyconError` (the base class).

## Account ownership: the first pairing needs a cookie

Every device on the hub belongs to an account. Pairing decides the account like this (the rules are in [docs/spec/hub.md](https://github.com/anycon-ai/anycon/blob/main/docs/spec/hub.md) § 4, under "cross-account" and "daemon's first identity"):

- Whichever side already has an account, the host (daemon) or the joiner (SDK), decides. If the two sides have different accounts → `PairingFailed('forbidden')`.
- The SDK is the joiner and, like the CLI, has **no browser cookie**. So when the **daemon has paired before** (it is a registered device on the hub, with an account), `pair()` just works, and the new device is added to the daemon's account.
- A **brand-new daemon** (its first pairing after `anycon init`, with no account on the hub yet) plus an SDK without a cookie means neither side has an account → the hub replies `PairFailed { reason: 'forbidden' }` and stores nothing.

The most common approach is to do the first pairing in a browser (the Web UI's sign-in cookie puts the daemon into your account). For scripts or CI with no browser, use `headers` to send a sign-in cookie and pair the daemon into the account first. Get the cookie from `/auth/token`:

```bash
curl -si -X POST "$HUB/auth/token" -H 'X-Anycon: 1' -H 'Content-Type: application/json' \
  -d "{\"token\":\"$ANYCON_ADMIN_TOKEN\"}" | grep -i '^set-cookie: anycon_session='
```

```ts
// 1) The first time: with the cookie (run `anycon pair --json` on the node to get the code).
//    This SDK device also becomes the first paired device.
await pair(hub, { code, name: 'bootstrap', storage, headers: { Cookie: 'anycon_session=…' } })
// 2) Any new device after that (run `anycon pair` on the node again): no cookie needed
await pair(hub, { code: code2, name: 'ci-runner' })
```

`headers` (accepted by `pair()`, `connect()` and `Client.open()`) are HTTP headers for the WebSocket upgrade request, and they **only take effect in Node / Bun**. The browser's native WebSocket can't set headers (the browser sends same-origin cookies itself), so they are ignored there. You can also use them for headers that a reverse proxy in front of the hub requires (such as Cloudflare Access's `CF-Access-Client-Id`). Paired devices authenticate with a device token, so `connect()` doesn't need a cookie.

## Storage

The device identity and paired nodes are stored locally, behind a minimal key-value interface, `SdkStorage { get, set, delete }`:

| Backend | Used for | Location |
|---|---|---|
| `FileStorage(dir)` | Default in Node / Bun | `$ANYCON_HOME/sdk/<hub-host>/<profile>/` (`ANYCON_HOME` defaults to `~/.anycon`); directory mode 0700, file mode 0600, atomic writes |
| `IndexedDBStorage(scope)` | Default in browsers | Database `anycon-sdk:<hub-host>/<profile>` |
| `MemoryStorage` | Tests / one-off scripts | In process |

`profile` defaults to `default`. Switch it with `ANYCON_PROFILE` or `{ profile }`. The profile becomes a directory name, so only letters, digits and `. _ -` are allowed (it must start with a letter or digit, be at most 64 characters, and not contain `..`). Path separators, `..` or an empty string throw `invalid_profile`. An empty `ANYCON_HOME` is treated as unset. In CI you can inject credentials without writing to disk: `ANYCON_DEVICE_KEY` (the 32-byte private key, hex) + `ANYCON_DEVICE_TOKEN`, plus the node to connect to, `ANYCON_NODE_ID` + `ANYCON_NODE_KEY` (the daemon's public key, hex).

## Large messages and transport desync

The hub limits the size of a single envelope (256 KiB by default), and a page of `session.history` or a single `tool_result` can be larger. So the SDK encrypts each JSON-RPC message and then uses `frame` (`@anycon/protocol`, ADR-30) to split it into `data` envelopes of ≤ 200 KiB, which the peer reassembles and then decrypts. This works the same in both directions. Noise nonces are sequential, so **losing any fragment desynchronizes the transport**. Therefore:

- For a paired `(from, to)`, the hub only forwards an envelope or bounces it back with `backpressure { to, seq }`. It **never drops one silently**, and bounces are "sticky": once a seq is bounced, the hub lets through only the smallest bounced seq (moving on only after it is resent successfully) and bounces everything else ([docs/protocol.md](https://github.com/anycon-ai/anycon/blob/main/docs/protocol.md) § 3.1). So the SDK keeps the **raw bytes** of recently sent envelopes (capped by size: 16 MiB by default, with a protocol minimum of 8 MiB; adjust it with `connect({ resendBufferBytes })`).
  - Envelopes sent less than 10 s ago, and bounced envelopes waiting to be resent, are **never evicted**. When the retained bytes go over the cap, the SDK **pauses sending new envelopes** instead of evicting (it treats the cap as an in-flight window and continues once the oldest batch is older than 10 s).
  - On `backpressure`, the SDK **pauses** sending anything new to that node, collects the bounced seqs, and waits `retry_after_ms` (from the latest `rate_limited`, capped at 30 s; otherwise 100–300 ms plus jitter, doubling on consecutive bounces up to about 2.4 s). It then resends every bounced seq **in ascending order, byte for byte** (without re-encrypting), and then sends the queued new requests. If envelopes are bounced again during the resend, it runs another round.
  - If the hub reports a seq the SDK didn't keep (never sent, or not from the current transport), the order can't be guaranteed: the SDK drops the Transport and does a new IK handshake (in-flight requests fail with `NodeOffline`).
  - When a `handshake` envelope is bounced, the SDK waits `retry_after_ms` (otherwise 300–1000 ms plus jitter, doubling on consecutive bounces, capped at 30 s) and then handshakes again, without resending right away or resetting.

  This is how 5 MiB and 9 MiB `fs.read` replies, and sustained traffic of tens of MiB, still arrive in order.
- On a decryption failure, an out-of-order / oversized / malformed fragment, or a `RESET` frame from the daemon, the SDK drops the Transport (in-flight requests fail with `NodeOffline`) and does a new IK handshake right away. Later requests work as usual.
- The hub's `dropped { seq, reason }` arrives at most once per connection per second. It signals that something wrong was sent and is not a per-envelope receipt, so the SDK **doesn't resend on it**: `node.on('error', …)` receives an `AnyconError` (`envelope_too_large` means still over the limit after fragmenting, i.e. the hub's `ANYCON_MAX_ENVELOPE_BYTES` is smaller than one fragment; `not_paired` becomes `NodeRejected`), in-flight requests fail, and the channel handshakes again.
- The hub's `rate_limited { retry_after_ms }` is a "how long to pause" hint for the **whole hub connection** (all nodes, resends included). It comes with `backpressure`, is capped at 30 s, resets on reconnect, and **doesn't mean an envelope was dropped**. Until it expires, new envelopes for every node are queued, and bounced seqs also wait until then to be resent. It doesn't fail requests, start a new handshake, or emit an `error` event, and the SDK doesn't treat the transport as desynchronized just because data was sent within the last second.

## Development

```bash
bun run --cwd packages/sdk test         # bun test
bun run --cwd packages/sdk test:node    # the same tests on Node: node --import tsx --conditions source --test
bun run --cwd packages/sdk typecheck
bun run --cwd packages/sdk build        # builds @anycon/protocol first, then tsc → dist/
```

`just test-sdk-real` runs end-to-end tests against a **real** `anycon-hub` + `anycon daemon` (mock agent). It builds them with `cargo build -p anycon-hub -p anycon-cli` and then runs `ANYCON_SDK_REAL=1 bun test tests/real.e2e.test.ts`; run `just gen-wasm` first. The tests use a temporary directory, random ports, and a fixed `ANYCON_ADMIN_TOKEN`:

- First, a `pair()` with the admin cookie pairs the daemon into the account. Then a regular `pair()` → `connect()` → `info` / `agents.list` / `projects.create` / `sessions.create` → `send('run permission')` (permissions, `tool_call`, usage) → `history()` cross-check → `setSettings` → `cancel()`.
- The last three cases cover large messages and heavy traffic: a 5 MiB `fs.read` reply arrives byte for byte; after 10 × 3 MiB in a row (30 MiB in total), the node is still online with no 30-second timeout; and the SDK, as the sender, sends 13 × 900 KiB `session.send` calls back to back, goes over the client budget of 10 MiB per minute, then pauses, waits, and resends until all of them complete without disconnecting. These rely on the hub's sticky rule and per-connection-kind rate budgets, and on the daemon resending in ascending order. On failure, the error includes the tail of the hub / daemon logs.

The real-stack tests are skipped by default. They run on Node too: `ANYCON_SDK_REAL=1 node --import tsx --conditions source --test tests/real.e2e.test.ts`. Override the binary directory with `ANYCON_BIN_DIR` (default `target/debug`).

The other, fast tests use two fakes:

- `tests/fake-hub.ts`: a real WebSocket server that only forwards. It implements Auth / `PairBegin { match_id }` / `PairJoin { match_id }` / forwarding / undeliverable / sticky backpressure per `(from, to)` (an `interceptor` returning `'backpressure'` / `'rate_limited'` bounces an envelope, and every later envelope is bounced until the bounced seq is resent successfully; `arrivals` / `bounced` record the order of arrivals and bounces) / `dropped`, following the real hub's wire conventions: permanent ids are assigned only at `PairDone`, temporary connections can only send `PairJoin` / `PairConfirm`, and envelopes over `maxEnvelopeBytes` get `dropped { too_large }`. It can also misbehave (`forgePairDoneOnConfirm`, `tamperPairDonePeer`) to test that pairing can't be fooled by a man in the middle.
- `tests/fake-daemon.ts`: a Noise responder written in TS. It generates the pairing code locally and handles `pair.ack`, `frame` fragmentation / reassembly / RESET, scripted `node.info` / `agent.list` / `workspace.*` / `session.*` / `permission.*` / `test.echo`, and `blob.*` following the real daemon's rules, with `blobChunkSize`, `maxAttachmentBytes`, `blobRefusal` (`quota` / `disk_full`), `chunkHook` (delays a chunk or makes it fail), and `chunksPeak` (the number of chunks being processed at once) for the attachment tests.

Every scenario runs once against each of two Noise implementations: `tests/mock-crypto.ts` (a toy implementation shaped like real Noise, always available) and the real wasm (present in `packages/protocol/wasm/` after `just gen-wasm`; if it's missing, a notice is printed and only the mock runs).

## License

[Apache-2.0](https://github.com/anycon-ai/anycon/blob/main/LICENSE)
