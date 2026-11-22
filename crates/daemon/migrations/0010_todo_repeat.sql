-- 周期项：带 repeat_kind 的项是模板，repeat_last_ms 是最近一次已经处理过的规则时间（同一次只复制一次，重启也一样）。
ALTER TABLE todos ADD COLUMN repeat_kind TEXT;
ALTER TABLE todos ADD COLUMN repeat_at TEXT;
ALTER TABLE todos ADD COLUMN repeat_weekday INTEGER;
ALTER TABLE todos ADD COLUMN repeat_last_ms INTEGER;
-- 复制出来的项记着模板的 id（source.todo_id）。
ALTER TABLE todos ADD COLUMN source_todo TEXT;
CREATE INDEX todos_source_todo ON todos(source_todo) WHERE source_todo IS NOT NULL;
