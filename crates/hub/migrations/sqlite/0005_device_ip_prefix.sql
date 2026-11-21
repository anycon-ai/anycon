-- 设备最近一次被看到时的 IP 前缀（docs/spec/hub.md § 5、§ 8）。元数据，不是会话内容。
--
--   - last_ip_prefix：和 last_seen_ms 同时写，取自那条连接的对端 IP，只存前缀（IPv4 /24，IPv6 /48，
--     和 web_sessions.ip_prefix、hub_audit.ip_prefix 同一个格式），不存完整 IP。每次写都覆盖（只有「最近一次」，不是历史）。
--     还没被看到过的设备（包括升级前就有、之后没再连过的）是 NULL。设备行在它就在；删账号时随设备行一起删除。
ALTER TABLE devices ADD COLUMN last_ip_prefix TEXT;
