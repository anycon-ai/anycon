-- 周期项的跨次状态（docs/spec/daemon.md § 26.11）：模板上的两个开关，和一张按模板存的「键 → 值」小表。

-- 开着时，模板复制出来的各次会话里的 agent 能读写这个模板的状态；只由人设置。
ALTER TABLE todos ADD COLUMN repeat_keep_state INTEGER NOT NULL DEFAULT 0;
-- 开着状态时，派发的第一条消息里不带 daemon 的用法说明（用户自己在正文里写）。缺省 0 = 带。
ALTER TABLE todos ADD COLUMN repeat_state_hint_off INTEGER NOT NULL DEFAULT 0;

-- 状态属于模板：复制出来的项读写的是模板这一份。删除模板（包括保留期清理）时一起删。
-- `key` / `value` 是用户内容（不进日志和审计），daemon 不解释它们；`key` 按字节比较（BINARY）。
-- `seq` 是这个模板内每次写入递增的序号，满了按它淘汰最久没被写过的。
CREATE TABLE todo_state (
  template_id TEXT    NOT NULL,
  key         TEXT    NOT NULL,
  value       TEXT    NOT NULL,
  written_ms  INTEGER NOT NULL,
  seq         INTEGER NOT NULL,
  PRIMARY KEY (template_id, key)
) WITHOUT ROWID;
CREATE INDEX todo_state_by_seq ON todo_state(template_id, seq);
