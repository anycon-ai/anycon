-- dev server 预览（ADR-39，docs/spec/hub.md § 13.7）。全部是元数据：hub 知道哪个账号的哪台节点有哪些 label、
-- 谁什么时候申请了令牌，不知道 label 对应哪个端口，令牌只存哈希。

-- 预览 label 绑定的节点。第一次有人为这个 label 申请令牌时绑定（先到先得），之后只能是同一个节点。
-- 设备只吊销、不物理删除，所以吊销节点时要显式删（见 `Store::revoke_device`）；这里的外键只管账号被删的情形。
CREATE TABLE preview_labels (
    label         TEXT PRIMARY KEY,
    node_id       TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_preview_labels_node ON preview_labels(node_id);

-- 预览令牌（只存哈希，24 小时有效）。device_id 是签发它的设备：设备被吊销时一并删除。
CREATE TABLE preview_tokens (
    token_hash    TEXT PRIMARY KEY,
    label         TEXT NOT NULL REFERENCES preview_labels(label) ON DELETE CASCADE,
    node_id       TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    device_id     TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_preview_tokens_device ON preview_tokens(device_id, created_at_ms);
CREATE INDEX idx_preview_tokens_node ON preview_tokens(node_id);
CREATE INDEX idx_preview_tokens_expires ON preview_tokens(expires_at_ms);
