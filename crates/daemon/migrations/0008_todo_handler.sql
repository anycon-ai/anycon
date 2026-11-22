-- 清单项的处理人；已有项由人处理。
ALTER TABLE todos ADD COLUMN handler TEXT NOT NULL DEFAULT 'me';
