#!/usr/bin/env python3
# 一次性调用夹具：记录参数与 stdin，按环境变量返回结果或制造进程故障。
import json
import os
import subprocess
import sys
import time

args = sys.argv[1:]
assert args.pop(0) == "prefix argument"
mode = os.environ.get("ONESHOT_MODE", "ok")
if mode == "blocked_stdin":
    time.sleep(30)
    sys.exit(0)
prompt = sys.stdin.read()
with open("request.json", "w") as f:
    json.dump({"args": args, "prompt": prompt, "cwd": os.getcwd(), "env": os.environ.get("ONESHOT_SECRET")}, f)
if mode == "sleep":
    subprocess.Popen([sys.executable, "-c", "import time; time.sleep(1.2); open('child-survived', 'w').close()"])
    with open("started", "w") as f:
        f.write(str(os.getpid()))
    time.sleep(30)
if mode == "exit":
    print("PRIVATE STDERR", file=sys.stderr)
    sys.exit(3)
if mode in ("stdout_limit", "stderr_limit"):
    print("x" * (1024 * 1024 + 1), file=sys.stdout if mode == "stdout_limit" else sys.stderr)
    sys.exit(0)
if mode == "malformed":
    print("PRIVATE INVALID JSON")
    sys.exit(0)
if args[0] == "exec":
    output = args[args.index("-o") + 1]
    if mode != "missing":
        with open(output, "w") as f:
            f.write("x" * (1024 * 1024 + 1) if mode == "file_limit" else "answer 中文\n")
    if mode in ("recovered_error", "error_only"):
        print(json.dumps({"type": "error", "message": "PRIVATE RETRY: Reconnecting... 1/1"}))
    if mode == "error":
        print(json.dumps({"type": "turn.failed", "error": {"message": "PRIVATE ERROR"}}))
    elif mode == "no_usage":
        print(json.dumps({"type": "turn.completed"}))
    elif mode not in ("error_only", "no_completion"):
        print(json.dumps({"type": "thread.started", "thread_id": "test"}))
        print(json.dumps({
            "type": "turn.completed",
            "usage": {"input_tokens": 10, "cached_input_tokens": 4, "output_tokens": 2},
        }))
    if mode == "completed_then_failed":
        print(json.dumps({"type": "turn.failed", "error": {"message": "PRIVATE ERROR"}}))
    if mode == "completed_nonzero":
        sys.exit(3)
else:
    result = {"result": "answer 中文\n", "is_error": mode == "error"}
    if mode != "no_usage":
        result.update(
            usage={
                "input_tokens": 10,
                "cache_creation_input_tokens": 3,
                "cache_read_input_tokens": 4,
                "output_tokens": 2,
                "cache_creation": {"ephemeral_5m_input_tokens": 3},
            },
            total_cost_usd=0.001,
        )
    print(json.dumps(result))
