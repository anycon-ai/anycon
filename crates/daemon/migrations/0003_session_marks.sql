-- 会话上的用户标注和已读水位（docs/spec/daemon.md § 11）。

-- `follow_up`：用户标了「待处理」；纯标注，不影响会话行为。
-- `read_seq`：用户已经看到的最大事件 seq，只增不减；会话不在运行且 `last_seq > read_seq` 时客户端显示未读。
ALTER TABLE sessions ADD COLUMN follow_up INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN read_seq INTEGER NOT NULL DEFAULT 0;

-- 升级前就存在的会话都当作已读，否则升级后每个会话都会亮未读。
UPDATE sessions SET read_seq = last_seq;
