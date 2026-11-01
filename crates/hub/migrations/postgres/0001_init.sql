-- anycon-hub 表结构，Postgres 版（docs/spec/hub.md § 8）。和 ../sqlite/0001_init.sql 一一对应：同样的表、列、约束、索引，
-- 编号和名字也一样；以后两边按同一个编号各加一份。全部是元数据：没有任何会话内容。
--
-- 类型映射（SQLite → Postgres → Rust 侧）：
--   TEXT                          → TEXT                                  → String
--   INTEGER（Unix 毫秒时间戳）     → BIGINT                                → i64（库里是 u64 的毫秒数，读出时 max(0)）
--   INTEGER（0 / 1 的布尔）        → BOOLEAN                               → bool
--   INTEGER（计数：attempts）      → INTEGER                               → i32
--   INTEGER PRIMARY KEY AUTOINCREMENT → BIGINT GENERATED ALWAYS AS IDENTITY → i64
-- 时间一律 Unix 毫秒，字段名以 `_ms` 结尾。`hub_audit.detail` 是 JSON 文本，保持 TEXT（不用 JSONB：hub 不解析它，
-- 原样存、原样取，键的顺序也不变）。
--
-- 排序：SQLite 的 TEXT 按字节比较，应用层（`util::ordered`、keyset 分页的游标）也按字节比较；Postgres 的默认排序规则随数据库的
-- locale 而变。需要和 Rust 一致的地方（`pairings` 的 CHECK、查询里的 `ORDER BY id`）显式写 `COLLATE "C"`。

-- 邮箱按小写存（应用层规范化），再加一个按 lower() 的唯一索引兜底；`email` 自带的 UNIQUE 保证同一个字符串只能有一个账号。
CREATE TABLE accounts (
    id            TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    email         TEXT UNIQUE,
    is_admin      BOOLEAN NOT NULL DEFAULT FALSE,
    created_at_ms BIGINT NOT NULL
);
CREATE UNIQUE INDEX idx_accounts_email_lower ON accounts(lower(email)) WHERE email IS NOT NULL;

-- 登录 token（只存哈希）。bootstrap token 来自配置，不在这里。
CREATE TABLE account_tokens (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    token_hash    TEXT NOT NULL UNIQUE,
    label         TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    last_used_ms  BIGINT
);
CREATE INDEX idx_account_tokens_account ON account_tokens(account_id);

-- 浏览器登录会话。id 是对外的不透明 id，token_hash 才是 cookie 值的哈希。
CREATE TABLE web_sessions (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    token_hash    TEXT NOT NULL UNIQUE,
    user_agent    TEXT NOT NULL DEFAULT '',
    ip_prefix     TEXT NOT NULL DEFAULT '',
    created_at_ms BIGINT NOT NULL,
    last_seen_ms  BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL
);
CREATE INDEX idx_web_sessions_account ON web_sessions(account_id);
CREATE INDEX idx_web_sessions_expires ON web_sessions(expires_at_ms);

CREATE TABLE invites (
    id            TEXT PRIMARY KEY,
    code_hash     TEXT NOT NULL UNIQUE,
    created_by    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    note          TEXT,
    created_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL,
    used_by       TEXT REFERENCES accounts(id) ON DELETE SET NULL,
    used_at_ms    BIGINT
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
    created_at_ms BIGINT NOT NULL,
    last_seen_ms  BIGINT,
    revoked_at_ms BIGINT
);
CREATE INDEX idx_devices_account ON devices(account_id);

-- 无向配对，存归一化顺序（device_a < device_b，按字节比较，和 `util::ordered` 一致）。
CREATE TABLE pairings (
    device_a      TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    device_b      TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    created_at_ms BIGINT NOT NULL,
    PRIMARY KEY (device_a, device_b),
    CHECK (device_a COLLATE "C" < device_b COLLATE "C")
);
CREATE INDEX idx_pairings_b ON pairings(device_b);

-- 邮箱验证码（ADR-37），只存哈希，10 分钟有效。同一组里最多同时有几条有效的码，新码不让旧码作废，超出时顶掉最旧的：
--   - 登录码（`purpose = 'login'`）：`account_id` 为空，一组 = 一个邮箱；
--   - 绑定码（`purpose = 'bind'`）：`account_id` 是发起绑定的账号，一组 = （账号，邮箱），别的账号的请求影响不到。
CREATE TABLE email_codes (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email         TEXT NOT NULL,
    purpose       TEXT NOT NULL,
    account_id    TEXT REFERENCES accounts(id) ON DELETE CASCADE,
    code_hash     TEXT NOT NULL,
    expires_at_ms BIGINT NOT NULL,
    -- 这一条码已经被尝试了几次（每次提交对每条有效的码各计一次）。
    attempts      INTEGER NOT NULL DEFAULT 0,
    created_at_ms BIGINT NOT NULL
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
    created_at_ms BIGINT NOT NULL
);

-- 通知偏好：每个账号每个类别走哪些渠道。没有行 = 用默认值（docs/spec/hub.md § 6.3）。
CREATE TABLE notification_prefs (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    category   TEXT NOT NULL,
    push       BOOLEAN NOT NULL,
    email      BOOLEAN NOT NULL,
    PRIMARY KEY (account_id, category)
);

-- hub 侧审计：账号层面的安全事件，只有元数据。保留 90 天。
CREATE TABLE hub_audit (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    ts_ms      BIGINT NOT NULL,
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
