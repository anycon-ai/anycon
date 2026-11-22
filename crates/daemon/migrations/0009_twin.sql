-- 范围按请求方保存；配置本身没有保留期。
CREATE TABLE twin_scopes (
    requester TEXT PRIMARY KEY,
    paused INTEGER NOT NULL,
    projects TEXT NOT NULL,
    agent TEXT,
    model TEXT,
    mode TEXT,
    max_sessions INTEGER NOT NULL,
    daily_dispatches INTEGER NOT NULL
);
CREATE TABLE twin_runs (
    id TEXT PRIMARY KEY,
    requester TEXT NOT NULL,
    started_ms INTEGER NOT NULL,
    finished_ms INTEGER,
    triggers TEXT NOT NULL,
    dry_run INTEGER NOT NULL,
    result TEXT NOT NULL,
    items TEXT NOT NULL,
    judge_calls TEXT NOT NULL,
    decisions TEXT NOT NULL,
    error TEXT
);
CREATE INDEX twin_runs_started ON twin_runs(requester, started_ms);
CREATE TABLE twin_questions (
    id TEXT PRIMARY KEY,
    todo_id TEXT NOT NULL,
    requester TEXT NOT NULL,
    question TEXT NOT NULL,
    options TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    answered_ms INTEGER,
    answer TEXT,
    answered_by TEXT,
    status TEXT NOT NULL
);
CREATE UNIQUE INDEX twin_questions_open ON twin_questions(todo_id) WHERE status = 'open';
CREATE INDEX twin_questions_created ON twin_questions(created_ms);
-- 派发来源独立于运行记录保留，旧会话中的人工消息仍能被识别为接手。
CREATE TABLE twin_sessions (
    session_id TEXT PRIMARY KEY,
    todo_id TEXT NOT NULL,
    requester TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX twin_sessions_created ON twin_sessions(requester, created_ms);
CREATE TABLE twin_items (
    todo_id TEXT PRIMARY KEY,
    failures INTEGER NOT NULL DEFAULT 0,
    last_outcome_key TEXT
);
