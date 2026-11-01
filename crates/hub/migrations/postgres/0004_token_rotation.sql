-- 设备 token 轮换（docs/spec/hub.md § 5），Postgres 版，和 ../sqlite/0004_token_rotation.sql 一一对应。
-- 全部是元数据，token 只以哈希形式出现。类型映射见 0001_init.sql。
--
--   - prev_token_hash / prev_token_expires_at_ms：轮换之前的那个 token 的哈希和它的宽限截止时间。
--     宽限期内认证时 `token_hash` 和 `prev_token_hash` 都接受；过了截止时间只认 `token_hash`。
--     每次轮换都覆盖它（最多保留「上一个」，不是历史）。
--   - token_rotated_at_ms：上一次轮换的时间，用来限制同一设备 1 分钟内最多轮换一次（没轮换过是 NULL）。
ALTER TABLE devices ADD COLUMN prev_token_hash TEXT;
ALTER TABLE devices ADD COLUMN prev_token_expires_at_ms BIGINT;
ALTER TABLE devices ADD COLUMN token_rotated_at_ms BIGINT;
CREATE INDEX idx_devices_prev_token ON devices(prev_token_hash) WHERE prev_token_hash IS NOT NULL;
