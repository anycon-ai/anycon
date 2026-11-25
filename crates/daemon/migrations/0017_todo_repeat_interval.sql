-- 周期项的间隔规则（repeat_kind = 'interval'）：生效的日子、间隔分钟数、当天的结束时间；以及 Bot 做完时自动关闭复制出的项。
ALTER TABLE todos ADD COLUMN repeat_days TEXT;
ALTER TABLE todos ADD COLUMN repeat_every_min INTEGER;
ALTER TABLE todos ADD COLUMN repeat_until TEXT;
ALTER TABLE todos ADD COLUMN repeat_auto_close INTEGER NOT NULL DEFAULT 0;
