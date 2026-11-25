/**
 * Bot 的判断后端替身：一个最小的 Anthropic Messages API 服务（docs/spec/daemon.md § 25.1：`POST /v1/messages`，
 * `x-api-key`、`anthropic-version`，请求体 `{ model, max_tokens, system, messages: [{ role: 'user', content }] }`，
 * 回应取 `content[]` 里第一个 `type = text` 的文字）。Node `http`，没有额外依赖，在用例自己的进程里起、监听 127.0.0.1 的随机端口。
 *
 * 每个请求都按 daemon 实际发的格式解析并记下来（`requests`），回答由用例给：`respond` 是一个按请求内容决定回答的函数，返回
 * 分拣计划（`{ items: [...] }`，§ 26.4）。项 id 要从请求里找：每一项包在随机边界 `<DATA_ANYCON_TWIN_<32 位 hex>> … </DATA_…>`
 * 里（§ 26.4），`dataBlocks` / `itemsOf` / `nodeRowsOf` 照边界解析，不写死边界的值。
 */
import { createServer, type IncomingMessage, type Server } from 'node:http'
import type { AddressInfo } from 'node:net'

/** daemon 发来的一次请求（已按 Messages API 的格式解析）。 */
export interface JudgeRequest {
  /** 收到的顺序，从 0 开始。 */
  index: number
  receivedAt: number
  path: string
  model: string
  maxTokens: number
  system: string
  /** 唯一的一条 user 消息的文字。 */
  prompt: string
  apiKey: string | undefined
  anthropicVersion: string | undefined
}

/** 分拣输入里的一项（§ 26.4：`id` / `project` / `dispatches` / `activity` / `title` / `body` 加上它的结果和问答）。 */
export interface TwinItem {
  id: string
  project: string
  dispatches: number
  activity: string
  title: string
  body: string
  /** 这一段的全文（含之前的结果、问过的问题和回答）。 */
  text: string
}

/** 一项的决定（§ 26.4 的输出）。 */
export interface TwinPlanItem {
  id: string
  action: 'dispatch' | 'ask_user' | 'ask_advisor' | 'wait' | 'hand_back' | 'done'
  reason: string
  project?: string
  note?: string
  node?: string
  question?: string
  options?: string[]
  rules?: string[]
}

export type JudgeResponder = (
  request: JudgeRequest,
) => { items: TwinPlanItem[] } | string | Promise<{ items: TwinPlanItem[] } | string>

export interface FakeJudge {
  /** `base_url`：daemon 配置里写这个（回环地址上的 http 是允许的，§ 25.1）。 */
  url: string
  /** 收到的全部请求，按顺序。 */
  requests: JudgeRequest[]
  /** 换回答函数；缺省对每一项回 `wait`。 */
  respond: JudgeResponder
  stop: () => Promise<void>
}

/** 请求里是不是 Bot 的分拣（系统提示的开头，`crates/daemon/src/twin.rs` 的 `SYSTEM`）。 */
export function isTriage(request: JudgeRequest): boolean {
  return request.system.startsWith("You are the node's twin.")
}

const BLOCK = /<DATA_(ANYCON_TWIN_[0-9a-f]{32})>\n([\s\S]*?)\n<\/DATA_\1>/g

/** 提示里被随机边界包起来的各段（项的段、节点表）；边界取请求里写的，不写死。 */
export function dataBlocks(request: JudgeRequest): { marker: string; text: string }[] {
  return [...request.prompt.matchAll(BLOCK)].map((m) => ({ marker: m[1]!, text: m[2]! }))
}

/** 分拣输入里的各项（以 `id: ` 开头的段）。 */
export function itemsOf(request: JudgeRequest): TwinItem[] {
  const items: TwinItem[] = []
  for (const { text } of dataBlocks(request)) {
    const head = /^id: (\S+)\nproject: (.*)\ndispatches: (\d+)\nactivity: (.*)\n/.exec(text)
    if (!head) continue
    const title = /^title: (.*)$/m.exec(text)?.[1] ?? ''
    const body = /^body:\n([\s\S]*?)(?:\n\n|$)/m.exec(text)?.[1] ?? ''
    items.push({
      id: head[1]!,
      project: head[2]!,
      dispatches: Number(head[3]),
      activity: head[4]!,
      title,
      body,
      text,
    })
  }
  return items
}

/** 节点表（§ 27.4：每个节点一行 JSON，在同一个随机边界里）；没有 Bot 连接时是 `undefined`。 */
export function nodeRowsOf(request: JudgeRequest): Record<string, unknown>[] | undefined {
  const block = dataBlocks(request).find(
    (b) => b.text.startsWith('{') && !b.text.startsWith('id: '),
  )
  if (!block) return undefined
  return block.text
    .split('\n')
    .filter(Boolean)
    .map((line) => JSON.parse(line) as Record<string, unknown>)
}

/** 缺省的回答：每一项都等着。 */
export function waitForAll(request: JudgeRequest): { items: TwinPlanItem[] } {
  return {
    items: itemsOf(request).map((i) => ({
      id: i.id,
      action: 'wait' as const,
      reason: 'e2e: wait',
    })),
  }
}

async function readBody(req: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = []
  for await (const chunk of req) chunks.push(chunk as Buffer)
  return Buffer.concat(chunks).toString('utf8')
}

/** 起一个假的判断后端。 */
export async function startJudge(respond: JudgeResponder = waitForAll): Promise<FakeJudge> {
  const requests: JudgeRequest[] = []
  const judge: FakeJudge = { url: '', requests, respond, stop: async () => undefined }
  const server: Server = createServer((req, res) => {
    void (async () => {
      const raw = await readBody(req)
      if (req.method !== 'POST' || req.url !== '/v1/messages') {
        res.writeHead(404).end()
        return
      }
      const body = JSON.parse(raw) as {
        model: string
        max_tokens: number
        system: string
        messages: { role: string; content: string }[]
      }
      const header = (name: string): string | undefined => {
        const v = req.headers[name]
        return Array.isArray(v) ? v[0] : v
      }
      const request: JudgeRequest = {
        index: requests.length,
        receivedAt: Date.now(),
        path: req.url,
        model: body.model,
        maxTokens: body.max_tokens,
        system: body.system,
        prompt: body.messages[0]?.content ?? '',
        apiKey: header('x-api-key'),
        anthropicVersion: header('anthropic-version'),
      }
      requests.push(request)
      const answer = await judge.respond(request)
      const text = typeof answer === 'string' ? answer : JSON.stringify(answer)
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(
        JSON.stringify({
          id: `msg_e2e_${request.index}`,
          type: 'message',
          role: 'assistant',
          model: body.model,
          content: [{ type: 'text', text }],
          stop_reason: 'end_turn',
          usage: { input_tokens: 100, output_tokens: 20 },
        }),
      )
    })().catch(() => {
      if (!res.headersSent) res.writeHead(500)
      res.end()
    })
  })
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
  judge.url = `http://127.0.0.1:${(server.address() as AddressInfo).port}`
  judge.stop = () =>
    new Promise<void>((resolve) => {
      server.closeAllConnections()
      server.close(() => resolve())
    })
  return judge
}
