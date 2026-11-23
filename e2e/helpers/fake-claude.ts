/**
 * 让一个额外节点（`startExtraDaemon`）的 `claude-code` 指向适配器测试用的假 `claude`
 * （`crates/adapters/tests/bin/fake-claude`，Python 3，不联网、没有模型）。
 *
 * 为什么不用 mock agent：分身派出的会话固定带「不 push、不开 PR」（docs/spec/daemon.md § 26.2），`twin.set` 会检查所选
 * agent 声明了 `no_push` 选项，而 mock 没有声明，所以只有 mock 的节点配置不了分身。假 `claude` 走的是真的 Claude Code
 * 适配器：探测、`no_push`、`auto` 模式（无人值守）、stream-json 会话都是真的代码路径，只是回答来自一份固定的脚本。
 *
 * 脚本里每一轮是 `system init` + 一条助手消息 + `result`；假 `claude` 每收到一条用户消息放一轮，所以脚本里写多轮，
 * 派发的第一轮和之后「接手」发的消息各用一轮。每个会话是一个新的假 `claude` 进程，都从第一轮开始。
 */
import { writeFileSync } from 'node:fs'
import { join } from 'node:path'

import { repoRoot, type E2EState } from './env.js'
import type { ExtraDaemonOptions } from './daemon.js'

const FAKE_CLAUDE = join(repoRoot, 'crates', 'adapters', 'tests', 'bin', 'fake-claude')
/** 假 `claude` 的固定会话 id（它会换成启动参数里的）。 */
const SID = '11111111-2222-3333-4444-555555555555'
const TURNS = 8

function turn(n: number, reply: string): string[] {
  const id = (k: number): string =>
    `00000000-0000-4000-8000-${String(n * 10 + k).padStart(12, '0')}`
  return [
    JSON.stringify({
      type: 'system',
      subtype: 'init',
      cwd: '/work/proj',
      session_id: SID,
      tools: ['Bash', 'Edit', 'Write', 'Read'],
      mcp_servers: [],
      model: 'claude-haiku-5-5',
      permissionMode: 'default',
      slash_commands: ['compact'],
      apiKeySource: 'none',
      claude_code_version: '2.1.293',
      output_style: 'default',
      capabilities: ['interrupt_receipt_v1', 'msg_lifecycle_v1', 'ui_surface_v1'],
      uuid: id(1),
    }),
    JSON.stringify({
      type: 'assistant',
      message: {
        model: 'claude-haiku-5-5',
        id: `msg_e2e_${n}`,
        type: 'message',
        role: 'assistant',
        content: [{ type: 'text', text: reply }],
        stop_reason: null,
        usage: { input_tokens: 2, output_tokens: 4 },
      },
      parent_tool_use_id: null,
      session_id: SID,
      uuid: id(2),
    }),
    JSON.stringify({
      type: 'result',
      subtype: 'success',
      is_error: false,
      duration_ms: 20,
      duration_api_ms: 10,
      num_turns: 1,
      result: reply,
      stop_reason: 'end_turn',
      session_id: SID,
      total_cost_usd: 0,
      usage: {
        input_tokens: 2,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        output_tokens: 4,
      },
      modelUsage: {},
      permission_denials: [],
      terminal_reason: 'completed',
      uuid: id(3),
    }),
  ]
}

/**
 * `startExtraDaemon` 的选项片段：打开 `claude-code` 并指向假 `claude`，每一轮的回答都是 `reply`。
 * 用法：`startExtraDaemon(state, name, { ...fakeClaude(state, 'done'), toml })`（`env` / `config` 要自己合并时展开两个字段）。
 */
export function fakeClaude(
  state: Pick<E2EState, 'tmpDir'>,
  reply: string,
): Required<Pick<ExtraDaemonOptions, 'env' | 'config'>> {
  const script = join(state.tmpDir, `fake-claude-${Math.random().toString(16).slice(2, 10)}.jsonl`)
  const lines = Array.from({ length: TURNS }, (_, n) => turn(n, reply)).flat()
  writeFileSync(script, `${lines.join('\n')}\n`)
  return {
    env: { FAKE_CLAUDE_SCRIPT: script },
    config: {
      'agents.claude-code.enabled': 'true',
      'agents.claude-code.command': FAKE_CLAUDE,
    },
  }
}
