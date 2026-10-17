-- anycon-hub 表结构（docs/spec/hub.md § 8）。
-- 全部是元数据：没有任何会话内容。时间一律 Unix 毫秒。

-- 邮箱按小写存（应用层规范化），再加一个按 lower() 的唯一索引兜底；`email` 自带的 UNIQUE 保证同一个字符串只能有一个账号。
CREATE TABLE accounts (
    id            TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    email         TEXT UNIQUE,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_accounts_email_lower ON accounts(lower(email)) WHERE email IS NOT NULL;

-- 登录 token（只存哈希）。bootstrap token 来自配置，不在这里。
CREATE TABLE account_tokens (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    token_hash    TEXT NOT NULL UNIQUE,
    label         TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    last_used_ms  INTEGER
);
CREATE INDEX idx_account_tokens_account ON account_tokens(account_id);

-- 浏览器登录会话。id 是对外的不透明 id，token_hash 才是 cookie 值的哈希。
CREATE TABLE web_sessions (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    token_hash    TEXT NOT NULL UNIQUE,
    user_agent    TEXT NOT NULL DEFAULT '',
    ip_prefix     TEXT NOT NULL DEFAULT '',
    created_at_ms INTEGER NOT NULL,
    last_seen_ms  INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_web_sessions_account ON web_sessions(account_id);
CREATE INDEX idx_web_sessions_expires ON web_sessions(expires_at_ms);

CREATE TABLE invites (
    id            TEXT PRIMARY KEY,
    code_hash     TEXT NOT NULL UNIQUE,
    created_by    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    note          TEXT,
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    used_by       TEXT REFERENCES accounts(id) ON DELETE SET NULL,
    used_at_ms    INTEGER
);

-- 设备。吊销 = revoked_at_ms 置位，不物理删除（保留审计）。
CREATE TABLE devices (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,
    name          TEXT NOT NULL,
    platform      TEXT NOT NULL DEFAULT '',
    origin        TEXT,
    token_hash    TEXT NOT NULL UNIQUE,
    created_at_ms INTEGER NOT NULL,
    last_seen_ms  INTEGER,
    revoked_at_ms INTEGER
);
CREATE INDEX idx_devices_account ON devices(account_id);

-- 无向配对，存归一化顺序（device_a < device_b）。
CREATE TABLE pairings (
    device_a      TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    device_b      TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (device_a, device_b),
    CHECK (device_a < device_b)
);
CREATE INDEX idx_pairings_b ON pairings(device_b);

-- 邮箱验证码（ADR-37），只存哈希，10 分钟有效。同一组里最多同时有几条有效的码，新码不让旧码作废，超出时顶掉最旧的：
--   - 登录码（`purpose = 'login'`）：`account_id` 为空，一组 = 一个邮箱；
--   - 绑定码（`purpose = 'bind'`）：`account_id` 是发起绑定的账号，一组 = （账号，邮箱），别的账号的请求影响不到。
CREATE TABLE email_codes (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    email         TEXT NOT NULL,
    purpose       TEXT NOT NULL,
    account_id    TEXT REFERENCES accounts(id) ON DELETE CASCADE,
    code_hash     TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    -- 这一条码已经被尝试了几次（每次提交对每条有效的码各计一次）。
    attempts      INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_email_codes_group ON email_codes(email, purpose);
CREATE INDEX idx_email_codes_expires ON email_codes(expires_at_ms);

-- Web Push 订阅（ADR-37），按设备（浏览器）存：一台设备一条，新的顶掉旧的；设备被吊销时一并删除。
-- endpoint / 密钥只用来加密并投递推送。
CREATE TABLE push_subscriptions (
    id            TEXT PRIMARY KEY,
    device_id     TEXT NOT NULL UNIQUE REFERENCES devices(id) ON DELETE CASCADE,
    endpoint      TEXT NOT NULL,
    p256dh        TEXT NOT NULL,
    auth          TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

-- 通知偏好：每个账号每个类别走哪些渠道。没有行 = 用默认值（docs/spec/hub.md § 6.3）。
CREATE TABLE notification_prefs (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    category   TEXT NOT NULL,
    push       INTEGER NOT NULL,
    email      INTEGER NOT NULL,
    PRIMARY KEY (account_id, category)
);

-- hub 侧审计：账号层面的安全事件，只有元数据。保留 90 天。
CREATE TABLE hub_audit (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms      INTEGER NOT NULL,
    event      TEXT NOT NULL,
    account_id TEXT,
    device_id  TEXT,
    ip_prefix  TEXT,
    user_agent TEXT,
    detail     TEXT
);
CREATE INDEX idx_hub_audit_ts ON hub_audit(ts_ms);

CREATE TABLE kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
