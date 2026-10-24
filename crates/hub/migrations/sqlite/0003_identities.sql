-- OAuth 登录（GitHub、Google）：账号的外部身份，以及授权流程里的一次性 state。全部是元数据：
-- hub 只记 (提供方, 提供方的稳定用户 id) 属于哪个账号，不存用户名、邮箱、头像，更不存 access token。

-- 一个账号在一个提供方最多一个身份（UNIQUE (account_id, provider)），一个外部身份只属于一个账号（主键）。
-- 账号被删时一并删除。
CREATE TABLE identities (
    provider      TEXT NOT NULL,
    subject       TEXT NOT NULL,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (provider, subject),
    UNIQUE (account_id, provider)
);

-- 一次授权流程的服务端状态，10 分钟有效，回调时原子地取走（单次使用）。
--   - state_hash：state 的哈希（state 本身只在浏览器的 cookie 和 URL 里）；
--   - purpose：`login` | `link`（account_id 是发起关联的账号）| `invite`（invite_hash 是邀请码的哈希，只有 single）；
--   - pkce_verifier：PKCE 的 code_verifier，换 token 时要原样带给提供方。
CREATE TABLE oauth_states (
    state_hash    TEXT PRIMARY KEY,
    provider      TEXT NOT NULL,
    purpose       TEXT NOT NULL,
    account_id    TEXT REFERENCES accounts(id) ON DELETE CASCADE,
    invite_hash   TEXT,
    next_path     TEXT,
    pkce_verifier TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_oauth_states_expires ON oauth_states(expires_at_ms);
