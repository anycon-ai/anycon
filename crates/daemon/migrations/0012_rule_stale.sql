-- 规则的「过期」（docs/spec/daemon.md § 23.2、§ 26.10）。
-- `last_referenced_ms`：分拣的决定引用了这条规则、并且决定被执行的最近时间。
-- `stale`：生效的规则 60 天里没被注入也没被引用，daemon 标上它；过期的规则不注入、不计字数预算。
-- `active_since_ms`：最近一次变成生效（新建时就生效、之后改成生效、从过期恢复）的时间，过期的计时从这里或
-- 最近一次注入 / 引用里最晚的那个算起。已有的规则从迁移的这一刻起算，不会因为升级立刻过期。
ALTER TABLE rules ADD COLUMN last_referenced_ms INTEGER;
ALTER TABLE rules ADD COLUMN stale INTEGER NOT NULL DEFAULT 0;
ALTER TABLE rules ADD COLUMN active_since_ms INTEGER;
UPDATE rules SET active_since_ms = CAST(strftime('%s', 'now') AS INTEGER) * 1000 WHERE status = 'active';
