-- 分身设备派来的会话里有人接手时，每个会话只在「经过」里记一次：这一列是记下的时间（毫秒），空表示还没记。
ALTER TABLE twin_sessions ADD COLUMN taken_ms INTEGER;
