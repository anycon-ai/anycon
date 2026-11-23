-- 分身连接的 A 这一侧（docs/spec/daemon.md § 27.1、§ 27.4）。
-- 已配对的节点以 `twin/nodes.json`（和 CLI 的设备文件同一种格式）为准；这里只放它的元数据，启动时对账：文件里有、这里没有的补一行，
-- 这里有、文件里没有的删掉。`status` 是上一次 `twin.peer_status` 的 JSON（B 在线时刷新，B 不在线时用它）。
CREATE TABLE twin_links (
    node_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    grp TEXT,
    pubkey TEXT NOT NULL,
    paired_ms INTEGER NOT NULL,
    status TEXT
);
-- A 上的一项派到别的节点的远程项（`Todo.remote`，只追加），和从 B 取回结果的进度。
-- `fetched_seq`：已经从 B 上这一项的 `todo.history` 里处理到的 `seq`（只增不减，重连之后据此补取、不重复记）。
-- `activity`：B 上这一项最近一次知道的 `activity`（B 的 `todo.changed` 带来的）：NULL = 还没有收到过，`none` = B 说没有。
CREATE TABLE todo_remotes (
    todo_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    remote_todo_id TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    fetched_seq INTEGER NOT NULL DEFAULT 0,
    activity TEXT,
    PRIMARY KEY (todo_id, node_id, remote_todo_id)
);
CREATE INDEX todo_remotes_remote ON todo_remotes(node_id, remote_todo_id);
