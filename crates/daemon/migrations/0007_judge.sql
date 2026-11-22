-- 判断调用只存元数据，不保存提示、回应或密钥。
CREATE TABLE judge_calls (
    id TEXT PRIMARY KEY,
    position TEXT NOT NULL,
    backend TEXT NOT NULL,
    kind TEXT NOT NULL,
    model TEXT,
    started_ms INTEGER NOT NULL,
    finished_ms INTEGER,
    status TEXT,
    input_bytes INTEGER NOT NULL,
    output_bytes INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cost_usd REAL
);
CREATE INDEX judge_calls_started ON judge_calls(started_ms);

-- 不依赖会话或规则的外键，删除来源之后仍能查看保留期内的记录。
CREATE TABLE rule_extract_runs (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    trigger TEXT NOT NULL,
    device_id TEXT,
    status TEXT NOT NULL,
    started_ms INTEGER NOT NULL,
    finished_ms INTEGER,
    points INTEGER NOT NULL,
    from_seq INTEGER NOT NULL,
    to_seq INTEGER NOT NULL,
    added TEXT NOT NULL DEFAULT '[]',
    dropped TEXT NOT NULL DEFAULT '{"invalid":0,"duplicate":0,"full":0}',
    backend TEXT NOT NULL,
    error_code TEXT,
    error_status INTEGER,
    call_id TEXT
);
CREATE INDEX rule_extract_runs_session_started ON rule_extract_runs(session_id, started_ms);
CREATE INDEX rule_extract_runs_started ON rule_extract_runs(started_ms);
