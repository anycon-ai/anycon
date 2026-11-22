-- 分身派出的会话上一次轮询到的 PR 状态：重启后接着和它比，第一次看到某个 PR 只记基线、不算事件。
-- 只有结构化的状态，没有评审或评论的正文。
CREATE TABLE twin_pr (
    session_id TEXT PRIMARY KEY,
    number INTEGER NOT NULL,
    state TEXT NOT NULL,
    checks TEXT NOT NULL,
    reviews INTEGER NOT NULL,
    last_review TEXT
);
