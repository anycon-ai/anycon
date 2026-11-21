-- 决策档案的规则（docs/spec/daemon.md § 23）：每个节点一份。
-- `text` 是用户内容：不进日志、不进审计。规则不受 `sessions.history_retention` 影响，daemon 从不自动删除。
CREATE TABLE rules (
    id               TEXT PRIMARY KEY,
    text             TEXT NOT NULL,
    project          TEXT,
    status           TEXT NOT NULL,
    sync             TEXT NOT NULL DEFAULT 'node',
    source_kind      TEXT NOT NULL,
    source_session   TEXT,
    source_seq       INTEGER,
    source_request   TEXT,
    source_device    TEXT,
    created_ms       INTEGER NOT NULL,
    updated_ms       INTEGER NOT NULL,
    last_injected_ms INTEGER
);

-- `rule.list` 和注入都按创建时间从早到晚。
CREATE INDEX rules_created ON rules (created_ms, id);
