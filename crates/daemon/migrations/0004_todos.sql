-- 清单（docs/spec/daemon.md § 21）：每个节点一份个人待办。
-- `title` / `body` 是用户内容：不进日志、不进审计。已关闭（done / dropped）的项按 `sessions.history_retention` 从 `closed_ms` 起清理。
CREATE TABLE todos (
    id             TEXT PRIMARY KEY,
    title          TEXT NOT NULL,
    body           TEXT NOT NULL DEFAULT '',
    project        TEXT,
    state          TEXT NOT NULL DEFAULT 'open',
    due_ms         INTEGER,
    source_kind    TEXT NOT NULL,
    source_device  TEXT,
    source_session TEXT,
    unreviewed     INTEGER NOT NULL DEFAULT 0,
    created_ms     INTEGER NOT NULL,
    updated_ms     INTEGER NOT NULL,
    closed_ms      INTEGER
);

-- `todo.list` 的排序和翻页：(updated_ms, id) 从新到旧。
CREATE INDEX todos_updated ON todos (updated_ms DESC, id DESC);

-- 两个上限的计数（没关闭的项、没被收下的项）每次 `todo.add` 都要数一遍：部分索引只收这些行，计数不用扫全表。
CREATE INDEX todos_open ON todos (state) WHERE state NOT IN ('done','dropped');
CREATE INDEX todos_unreviewed ON todos (id) WHERE unreviewed = 1;
