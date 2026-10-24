-- dev server 预览（ADR-39，docs/spec/daemon.md § 19.2）。

-- 每个端口一个固定的预览 label（26 位小写 base32，第一次 `preview.open` 这个端口时生成）：
-- 同一个端口的预览 origin 不变，dev app 存在浏览器里的状态下次还在。
CREATE TABLE preview_labels (
  port          INTEGER PRIMARY KEY,
  label         TEXT    NOT NULL UNIQUE,
  created_at_ms INTEGER NOT NULL
);

-- 预览票据：`ticket_id`（32 位 hex）和 `psk`（64 位 hex，Noise NKpsk0 的 PSK）都来自系统随机数。
-- 存库是为了 daemon 重启后预览页能直接重连；密钥和其他密钥一样只在数据目录里（不进日志、不进审计）。
-- `device_id` 是签发它的设备，吊销设备时按它整批删；`session_id` 只用于界面展示。
CREATE TABLE preview_tickets (
  ticket_id     TEXT    PRIMARY KEY,
  label         TEXT    NOT NULL,
  port          INTEGER NOT NULL,
  psk           TEXT    NOT NULL,
  device_id     TEXT    NOT NULL,
  session_id    TEXT,
  created_at_ms INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL
);
CREATE INDEX preview_tickets_label ON preview_tickets(label);
CREATE INDEX preview_tickets_device ON preview_tickets(device_id, port);
