#!/usr/bin/env bash
# 本地开发：hub + daemon（mock agent）+ web dev server，Ctrl+C 全部退出。
#
#   ./scripts/dev.sh            # 或 `just dev`
#
# 数据都在 .anycon-dev/（hub 的数据目录、daemon 的 ANYCON_HOME、workspace、日志），删掉这个目录就是重置。
# 可调：HUB_PORT（默认 8080）、DEV_NO_WEB=1（不起 vite，用 hub 嵌入的 web/dist）、DEV_REAL_CLAUDE=1 / DEV_REAL_CODEX=1（启用真实 Claude Code / Codex，和 mock 一起可选）。
# vite dev server 固定 :5173，监听 localhost（只有 IPv6 ::1 也可能），浏览器请用 http://localhost:5173。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

DEV="$ROOT/.anycon-dev"
HUB_PORT="${HUB_PORT:-8080}"
HUB_URL="http://127.0.0.1:${HUB_PORT}"
DAEMON_HOME="$DEV/daemon"
WORKSPACE="$DEV/workspace"
LOGS="$DEV/logs"
mkdir -p "$DEV/hub" "$WORKSPACE" "$LOGS"

PIDS=()

kill_tree() {
  local pid="$1" child
  for child in $(pgrep -P "$pid" 2>/dev/null || true); do
    kill_tree "$child"
  done
  kill "$pid" 2>/dev/null || true
}

cleanup() {
  trap - INT TERM EXIT
  [ "${#PIDS[@]}" -eq 0 ] && return 0
  echo
  echo "[dev] stopping..."
  local pid
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    kill_tree "$pid"
  done
  # 给 daemon 一点时间优雅退出（它会先让 agent 收尾）
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    local alive=0
    for pid in ${PIDS[@]+"${PIDS[@]}"}; do
      if kill -0 "$pid" 2>/dev/null; then alive=1; fi
    done
    [ "$alive" = 0 ] && break
    sleep 0.5
  done
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    kill -9 "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT
trap "exit 130" INT TERM

# 端口上已经有东西在应答（比如上一次没退干净的 hub）：别连到错误的 hub 上还以为成功了。
if curl -fsS --max-time 1 "$HUB_URL/healthz" >/dev/null 2>&1; then
  echo "[dev] something already answers on $HUB_URL/healthz; stop it first or set HUB_PORT=<other>" >&2
  exit 1
fi

if [ ! -f "$ROOT/packages/protocol/wasm/anycon_crypto.js" ]; then
  echo "[dev] warning: packages/protocol/wasm is missing, the web UI cannot pair; run \`just gen-wasm\` first" >&2
fi

echo "[dev] building hub and anycon (cargo build -p anycon-hub -p anycon-cli)..."
cargo build -p anycon-hub -p anycon-cli

HUB_BIN="$ROOT/target/debug/anycon-hub"
ANYCON_BIN="$ROOT/target/debug/anycon"

# ---------------------------------------------------------------- hub
echo "[dev] starting hub on $HUB_URL (log: $LOGS/hub.log)"
ANYCON_DATA_DIR="$DEV/hub" \
ANYCON_PUBLIC_URL="$HUB_URL" \
ANYCON_LISTEN="127.0.0.1:${HUB_PORT}" \
ANYCON_LOG="${ANYCON_LOG:-info}" \
  "$HUB_BIN" >"$LOGS/hub.log" 2>&1 &
PIDS+=("$!")

for i in $(seq 1 120); do
  if curl -fsS "$HUB_URL/healthz" >/dev/null 2>&1; then break; fi
  if ! kill -0 "${PIDS[0]}" 2>/dev/null; then
    echo "[dev] the hub exited; last log lines:" >&2
    tail -n 20 "$LOGS/hub.log" >&2
    exit 1
  fi
  sleep 0.5
done
curl -fsS "$HUB_URL/healthz" >/dev/null 2>&1 || { echo "[dev] the hub did not become healthy" >&2; exit 1; }

# ---------------------------------------------------------------- daemon
export ANYCON_HOME="$DAEMON_HOME"
if [ ! -f "$DAEMON_HOME/config.toml" ]; then
  echo "[dev] initializing the dev daemon in $DAEMON_HOME"
  "$ANYCON_BIN" init --hub "$HUB_URL" --workspace "$WORKSPACE" --name dev-node --yes
  # 开发默认只用 mock：内置的真实 agent（claude-code、codex）没有配置段也参与探测，本机装了就会成为默认 agent，所以都关掉。
  # 要试真实 agent：DEV_REAL_CLAUDE=1 / DEV_REAL_CODEX=1，或之后 `anycon config set agents.<id>.enabled true`
  "$ANYCON_BIN" config set agents.claude-code.enabled false >/dev/null
fi
# codex 没有配置段时（新建的、或 codex 支持之前建的 .anycon-dev）先关掉；已有的段是用户自己的选择，不动
if ! grep -q '^\[agents\.codex\]' "$DAEMON_HOME/config.toml"; then
  "$ANYCON_BIN" config set agents.codex.enabled false >/dev/null
fi
if [ "${DEV_REAL_CLAUDE:-0}" = "1" ]; then
  "$ANYCON_BIN" config set agents.claude-code.enabled true >/dev/null
fi
if [ "${DEV_REAL_CODEX:-0}" = "1" ]; then
  "$ANYCON_BIN" config set agents.codex.enabled true >/dev/null
fi
echo "[dev] starting the daemon with the mock agent (log: $LOGS/daemon.log)"
ANYCON_MOCK_AGENT=1 "$ANYCON_BIN" daemon >"$LOGS/daemon.log" 2>&1 &
PIDS+=("$!")
DAEMON_PID="$!"

for i in $(seq 1 120); do
  if [ -S "$DAEMON_HOME/daemon.sock" ]; then break; fi
  if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "[dev] the daemon exited; last log lines:" >&2
    tail -n 20 "$LOGS/daemon.log" >&2
    exit 1
  fi
  sleep 0.5
done
[ -S "$DAEMON_HOME/daemon.sock" ] || { echo "[dev] the daemon did not open $DAEMON_HOME/daemon.sock; see $LOGS/daemon.log" >&2; exit 1; }

# ---------------------------------------------------------------- web
if [ "${DEV_NO_WEB:-0}" != "1" ] && [ -f "$ROOT/web/package.json" ] && command -v bun >/dev/null 2>&1; then
  if [ ! -d "$ROOT/node_modules" ]; then
    echo "[dev] bun install..."
    bun install
  fi
  echo "[dev] starting the web dev server (log: $LOGS/web.log)"
  ANYCON_HUB_URL="$HUB_URL" bun run --cwd web dev >"$LOGS/web.log" 2>&1 &
  PIDS+=("$!")
  WEB_PID="$!"
  for i in $(seq 1 120); do
    if curl -fsS --max-time 1 "http://localhost:5173/" >/dev/null 2>&1; then break; fi
    if ! kill -0 "$WEB_PID" 2>/dev/null; then
      echo "[dev] the web dev server exited; last log lines:" >&2
      tail -n 20 "$LOGS/web.log" >&2
      exit 1
    fi
    sleep 0.5
  done
  WEB="http://localhost:5173"
else
  WEB="$HUB_URL (the hub's embedded web UI; web/ dev server not started)"
fi

ADMIN_TOKEN=""
if [ -f "$DEV/hub/admin-token" ]; then ADMIN_TOKEN="$(cat "$DEV/hub/admin-token")"; fi

cat <<MSG

------------------------------------------------------------------
 anycon dev environment is up

   hub     : $HUB_URL
   web     : $WEB
   admin token (paste it on the login page):
             $ADMIN_TOKEN

 Pair the dev daemon (enter the code in the web UI at /setup; the default code is 16 characters,
 --short gives an 8-character one, which is fine here because the hub is local):
   ANYCON_HOME=$DAEMON_HOME $ANYCON_BIN pair --short

 Talk to it from the terminal:
   ANYCON_HOME=$DAEMON_HOME $ANYCON_BIN new --agent mock "run permission"

 Real agents (Claude Code / Codex):
   ANYCON_HOME=$DAEMON_HOME $ANYCON_BIN config set agents.claude-code.enabled true
   ANYCON_HOME=$DAEMON_HOME $ANYCON_BIN config set agents.codex.enabled true

 Logs: $LOGS/{hub,daemon,web}.log      Reset everything: rm -rf $DEV
 Ctrl+C stops all.
------------------------------------------------------------------

MSG

# 任何一个子进程退出就整体退出（bash 3.2 没有 wait -n，轮询）
while true; do
  for pid in "${PIDS[@]}"; do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "[dev] a service exited unexpectedly (pid $pid); see $LOGS/*.log" >&2
      exit 1
    fi
  done
  sleep 1
done
