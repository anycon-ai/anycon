-- `twin.stats` 按时间范围读「经过」里某几种事件（状态关闭、退回、接手）。主键是 (todo_id, seq)，没有按时间的入口，
-- 90 天的统计会扫整张表（`result` 事件带最多 4 KiB 的文字）。
CREATE INDEX todo_events_kind_ts ON todo_events (kind, ts_ms);
