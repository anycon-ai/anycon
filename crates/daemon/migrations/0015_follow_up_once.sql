-- `follow_up` 的语义改了：从「待处理 / 完成后提醒」两用的标记，变成只提醒一次的「完成后提醒」，
-- 会话完成、发出通知时由 daemon 清掉（docs/decisions.md ADR-59）。
--
-- 旧语义下，停下来的会话上的 `follow_up = 1` 是「待处理」书签；按新语义它会被读成「下一轮结束后提醒」，
-- 下一轮结束时冷不丁来一条通知。所以升级时把不在进行中的会话的标记清掉；进行中的（queued / starting /
-- running / waiting_permission）本来就是「完成后提醒」，保留。
UPDATE sessions SET follow_up = 0
 WHERE status NOT IN ('running', 'starting', 'queued', 'waiting_permission');
