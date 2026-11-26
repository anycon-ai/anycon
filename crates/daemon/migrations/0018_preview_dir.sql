-- 目录预览（ADR-62，docs/spec/daemon.md § 19.6）：daemon 自己把一个项目目录（或 `.scratch`）里的文件按静态网站提供。

-- 每个目录一个固定的预览 label，和端口的 label（`preview_labels`）一样第一次 `preview.open` 时生成、之后不变。
-- `dir` 是 `preview.open` 的 `dir`：项目名，或 `.scratch`。项目改名之后是另一个目录、另一个 label。
CREATE TABLE preview_dir_labels (
  dir           TEXT    PRIMARY KEY,
  label         TEXT    NOT NULL UNIQUE,
  created_at_ms INTEGER NOT NULL
);

-- 目录票据：`dir` 不为空，`port` 记 0（端口票据的 `port` 总是 1–65535，`dir` 为空）。
ALTER TABLE preview_tickets ADD COLUMN dir TEXT;
CREATE INDEX preview_tickets_device_dir ON preview_tickets(device_id, dir);
