-- 派发（docs/spec/daemon.md § 22）：清单项和它派出的会话的关联，以及每一项的「经过」。
-- `todo_events.data` 里的 `text`（agent 一轮结束时的最后一段话）是会话内容：不进日志、不进审计。

-- 会话从哪一项派出。清单项被删之后原样留着（客户端按「来源已删除」显示）；会话行里没有外键，因为项和会话各自独立地删。
ALTER TABLE sessions ADD COLUMN todo_id TEXT;

-- 一项派出的会话，按派发的先后（`seq` 在一项内从 1 递增）。会话被删除，这一行留着；这一项被删除，这一项的行一起删。
CREATE TABLE todo_sessions (
    todo_id    TEXT NOT NULL,
    session_id TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    PRIMARY KEY (todo_id, session_id)
) WITHOUT ROWID;
CREATE INDEX todo_sessions_order ON todo_sessions (todo_id, seq);

-- 「经过」：只追加。`kind` 是 dispatched / result / state，`data` 是除 seq、ts_ms、kind 之外的字段（JSON）。
-- `session_id` 单独成列，是为了「最近 5 次派发各自最后一条 result」不用解析 JSON。
CREATE TABLE todo_events (
    todo_id    TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    ts_ms      INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    session_id TEXT,
    data       TEXT NOT NULL,
    PRIMARY KEY (todo_id, seq)
) WITHOUT ROWID;
CREATE INDEX todo_events_session ON todo_events (todo_id, session_id, kind, seq);
