-- anycon daemon 存储（docs/spec/daemon.md § 11）。时间一律 Unix 毫秒。

CREATE TABLE sessions (
  id               TEXT PRIMARY KEY,
  project          TEXT,
  -- 创建时项目指向哪里：关联项目 = `linked:<canonical 目录>`，根目录下的项目 = `folder:<名字>`，scratch 会话为空。
  -- 拉起 agent 前核对：项目现在指向的位置变了（同名重新关联到别的目录…）就拒绝，不在新目录里续接。
  project_loc      TEXT,
  agent            TEXT NOT NULL,
  title            TEXT NOT NULL DEFAULT '',
  title_locked     INTEGER NOT NULL DEFAULT 0,
  status           TEXT NOT NULL,
  archived         INTEGER NOT NULL DEFAULT 0,
  settings         TEXT NOT NULL DEFAULT '{}',
  agent_session_id TEXT,
  launch_args      TEXT,
  -- 会话自己的 worktree（ADR-34），见 `worktrees`
  worktree_id      TEXT,
  created_ms       INTEGER NOT NULL,
  updated_ms       INTEGER NOT NULL,
  last_seq         INTEGER NOT NULL DEFAULT 0,
  next_turn_id     INTEGER NOT NULL DEFAULT 1,
  usage            TEXT NOT NULL DEFAULT '{}',
  preview          TEXT
);
CREATE INDEX sessions_project ON sessions(project);
CREATE INDEX sessions_updated ON sessions(updated_ms);

-- kind / ref_id 是从 event JSON 里提出来的索引列：
--   user_message -> ref_id = message_id（幂等）；permission / permission_resolved -> ref_id = request_id
CREATE TABLE events (
  session_id TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  ts_ms      INTEGER NOT NULL,
  turn_id    INTEGER,
  kind       TEXT NOT NULL,
  ref_id     TEXT,
  event      TEXT NOT NULL,
  PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;
CREATE UNIQUE INDEX events_user_message ON events(session_id, ref_id) WHERE kind = 'user_message';
CREATE INDEX events_ref ON events(ref_id, kind) WHERE ref_id IS NOT NULL;

CREATE TABLE turns (
  session_id TEXT NOT NULL,
  turn_id    INTEGER NOT NULL,
  message_id TEXT NOT NULL,
  started_ms INTEGER NOT NULL,
  ended_ms   INTEGER,
  reason     TEXT,
  usage      TEXT,
  PRIMARY KEY (session_id, turn_id)
) WITHOUT ROWID;
CREATE INDEX turns_message ON turns(session_id, message_id);

CREATE VIRTUAL TABLE messages_fts USING fts5(
  text,
  session_id UNINDEXED,
  seq UNINDEXED,
  role UNINDEXED,
  ts_ms UNINDEXED,
  tokenize = 'unicode61'
);

-- 图片附件（ADR-35，docs/spec/daemon.md § 7.10）。`session_id` 为空 = 上传完成但还没被消息引用（24 小时后回收）；
-- `path` 存正式文件名（`<blob_id>.<ext>`），相对 `<数据目录>/blobs/`；`name` 是展示名。
CREATE TABLE blobs (
  id         TEXT PRIMARY KEY,
  session_id TEXT,
  name       TEXT NOT NULL DEFAULT '',
  mime       TEXT NOT NULL,
  size       INTEGER NOT NULL,
  width      INTEGER,
  height     INTEGER,
  sha256     TEXT NOT NULL,
  path       TEXT NOT NULL,
  created_ms INTEGER NOT NULL
);
CREATE INDEX blobs_session ON blobs(session_id);
CREATE INDEX blobs_created ON blobs(created_ms) WHERE session_id IS NULL;

CREATE TABLE peers (
  device_id    TEXT PRIMARY KEY,
  pubkey       TEXT NOT NULL,
  name         TEXT NOT NULL,
  kind         TEXT NOT NULL,
  paired_at_ms INTEGER NOT NULL,
  last_seen_ms INTEGER
);

CREATE TABLE agent_cache (
  agent_id    TEXT PRIMARY KEY,
  info        TEXT NOT NULL,
  diagnose    TEXT,
  probed_at_ms INTEGER NOT NULL
);

CREATE TABLE audit (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts_ms       INTEGER NOT NULL,
  device_id   TEXT NOT NULL,
  device_name TEXT NOT NULL,
  action      TEXT NOT NULL,
  target      TEXT,
  detail      TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_ts ON audit(ts_ms);
CREATE INDEX audit_device ON audit(device_id, ts_ms);

CREATE TABLE jobs (
  id         TEXT PRIMARY KEY,
  kind       TEXT NOT NULL,
  target     TEXT,
  status     TEXT NOT NULL,
  error      TEXT,
  started_ms INTEGER NOT NULL,
  ended_ms   INTEGER
);

CREATE TABLE kv (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);

-- 关联的本机已有目录（ADR-33）：项目名 -> canonicalize 之后的绝对路径。
-- 根目录下的项目不进这张表（文件夹即项目，元数据全部推导）。
CREATE TABLE linked_projects (
  name       TEXT PRIMARY KEY,
  path       TEXT NOT NULL UNIQUE,
  created_ms INTEGER NOT NULL
);

-- 每会话 worktree（ADR-34）。`worktrees` 是 anycon 自己建的 git worktree 的登记表：只删这里记过的目录，
-- 删之前还要逐段 lstat 并核对 (dev, ino)。会话删除后行可以留着（session_id 置空，等用户在节点设置里清理）。
CREATE TABLE worktrees (
  id           TEXT PRIMARY KEY,
  -- 创建时项目指向哪里，写法同 sessions.project_loc；项目改名时一起更新
  project_loc  TEXT,
  -- 项目名（展示用，和 workspace.list 里的一致）
  project      TEXT NOT NULL,
  -- 相对项目目录的路径，如 .anycon/worktrees/fix-login
  rel_path     TEXT NOT NULL,
  branch       TEXT NOT NULL,
  -- 规范化的引用名（`refs/heads/main`、`refs/tags/v1.0`）或完整提交 SHA
  base         TEXT NOT NULL,
  -- base 的展示名（`main`、`v1.0`、提交前 12 位）：客户端看到的、合并提交信息里用的都是它；为空时按 `base` 推
  base_display TEXT,
  base_sha     TEXT NOT NULL,
  session_id   TEXT,
  -- creating | active | kept | removed | missing | failed
  state        TEXT NOT NULL,
  -- 创建完成时目录的设备号 / inode（creating / failed 时为空）
  dev          INTEGER,
  ino          INTEGER,
  -- worktree 根下 `.git` 指针文件在创建时的内容（项目目录的绝对路径换成 `{project}`，见 `worktree::read_git_pointer`）。
  -- agent 的 cwd 就是 worktree，能改这个文件；拉起 agent 和每个 turn 开始前核对它没变，变了按「被换掉」处理。为空不核对
  git_pointer  TEXT,
  created_ms   INTEGER NOT NULL
);
CREATE INDEX worktrees_project ON worktrees(project);
CREATE INDEX worktrees_session ON worktrees(session_id);

-- 账号内接入的节点口令（ADR-36，docs/spec/daemon.md § 15.1）。最多一行（`id = 1`）：
-- `salt` 是 16 字节随机盐，`prs` 是 `Argon2id(NFC(口令), salt, m = 64 MiB, t = 3, p = 1, out = 32)`——口令本身不存。
-- `failures` 是连续失败次数（悲观计数：发出 CPace 份额之前先 +1，第 3 条握手消息校验通过后清零），
-- `next_allowed_at_ms` 是退避结束的时间（0 = 不在退避中），`locked` 为 1 表示连续 10 次失败后口令接入被锁定。
-- 清除口令 = 删这一行（计数一并清零）。接入会话和接入码只在内存里，不在这里。
CREATE TABLE enroll_password (
  id                 INTEGER PRIMARY KEY CHECK (id = 1),
  salt               BLOB    NOT NULL,
  prs                BLOB    NOT NULL,
  set_at_ms          INTEGER NOT NULL,
  set_by             TEXT    NOT NULL,
  failures           INTEGER NOT NULL DEFAULT 0,
  next_allowed_at_ms INTEGER NOT NULL DEFAULT 0,
  locked             INTEGER NOT NULL DEFAULT 0
);
