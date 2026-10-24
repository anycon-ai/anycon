/**
 * 预览用例的上游：一个最小的 dev server 替身（Node `http`，不引依赖），监听 127.0.0.1 的随机端口。
 * 服务在 globalSetup 的进程里，端口写进状态文件（`readState().upstreamPort`）；daemon 的 `preview.allow_ports` 里放的就是它
 * （端口不属于任何会话，所以要靠配置放行，docs/spec/daemon.md § 19.3）。
 *
 * 路径：
 *   GET  /                页面：标题、正文、一个同源 WebSocket 的回显、`document.cookie`、两份较大的脚本是否完整到达
 *   GET  /app.js          页面脚本：连同源 `/ws` 发一条消息，把回显写进 DOM；读 `document.cookie` 写进 DOM；挂 `window.e2e`
 *   GET  /big.js          约 600 KiB 的脚本，上游自己按 `accept-encoding` 做 gzip（daemon 原样转发 gzip）
 *   GET  /big-identity.js 同样的内容，上游永远不压缩（daemon 自己压缩，每段 sync flush）
 *   GET  /api/set-cookie  设两个 cookie：`e2e_plain`（页面脚本看得到）和 `e2e_secret`（HttpOnly）
 *   GET  /api/cookie      把收到的 `Cookie` 头回显出来
 *   GET  /api/headers     把上游收到的请求头回显出来（验证 daemon 的请求头改写）
 *   *    /api/echo        回显方法、路径和正文（验证请求体转发）
 *   GET  /redirect        302 到 `http://localhost:<端口>/landed`（验证 `location` 改写）
 *   GET  /landed          跳转的落点页
 *   GET  /ws              WebSocket 回显：文本回 `echo: <原文>`，二进制原样，ping 回 pong，close 回 close
 *
 * WebSocket 的服务端握手和帧只实现了够用的部分（不分片、不压缩扩展）。
 */
import { createHash } from 'node:crypto'
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http'
import type { Socket } from 'node:net'
import { gzipSync } from 'node:zlib'

export interface UpstreamServer {
  port: number
  close: () => Promise<void>
}

export const PAGE_TITLE = 'Anycon E2E Upstream'
export const PAGE_HEADING = 'Hello from the upstream dev server'
export const LANDED_HEADING = 'Landed after the redirect'
export const WS_GREETING = 'hello from the page'
export const WS_ECHO_PREFIX = 'echo: '

/** 较大脚本的随机内容字节数（base64 之后约 600 KiB，gzip 之后基本不再变小：必然跨很多帧）。 */
const BIG_RANDOM_BYTES = 450 * 1024

/** 固定种子的伪随机数（mulberry32）：globalSetup 和 worker 进程里算出同一份内容，用例据此算期望值。 */
function prng(seed: number): () => number {
  let a = seed >>> 0
  return () => {
    a = (a + 0x6d2b79f5) >>> 0
    let t = a
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

/** 浏览器里和这里用同一个算法给字符串算指纹：`h = h * 31 + charCode`，按 32 位有符号整数回绕。 */
function fingerprint(text: string): number {
  let h = 0
  for (let i = 0; i < text.length; i++) h = (Math.imul(h, 31) + text.charCodeAt(i)) | 0
  return h >>> 0
}

export interface BigScript {
  /** 整个脚本的文本。 */
  source: string
  /** 脚本里的数据串长度和指纹：脚本执行后把 `big: ok <长度> <指纹>` 写进 `#big`。 */
  length: number
  hash: number
  /** `#big` 里应当出现的文字。 */
  expected: string
}

let cachedBig: BigScript | undefined

/** 较大的脚本（确定的内容）。 */
export function bigScript(): BigScript {
  if (cachedBig) return cachedBig
  const rand = prng(0xa11c0)
  const bytes = Buffer.alloc(BIG_RANDOM_BYTES)
  for (let i = 0; i < bytes.length; i++) bytes[i] = Math.floor(rand() * 256)
  const data = bytes.toString('base64')
  const hash = fingerprint(data)
  const source = [
    '// E2E fixture: a large script, content is deterministic noise.',
    `var DATA = "${data}";`,
    'var h = 0;',
    'for (var i = 0; i < DATA.length; i++) h = (Math.imul(h, 31) + DATA.charCodeAt(i)) | 0;',
    `var out = 'big: ' + (DATA.length === ${data.length} && (h >>> 0) === ${hash} ? 'ok ' + DATA.length + ' ' + (h >>> 0) : 'corrupt ' + DATA.length + ' ' + (h >>> 0));`,
    "var el = document.getElementById('big');",
    'if (el) el.textContent = out;',
    '',
  ].join('\n')
  cachedBig = { source, length: data.length, hash, expected: `big: ok ${data.length} ${hash}` }
  return cachedBig
}

const PAGE = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>${PAGE_TITLE}</title>
</head>
<body>
<h1 id="heading">${PAGE_HEADING}</h1>
<p>origin: <span id="origin">pending</span></p>
<p>ws: <span id="ws-status">pending</span></p>
<p>ws echo: <span id="ws-echo"></span></p>
<p>cookies: <span id="cookies"></span></p>
<p id="big">big: pending</p>
<script src="/app.js"></script>
<script src="/big.js"></script>
</body>
</html>
`

const APP_JS = `(function () {
  var set = function (id, text) { document.getElementById(id).textContent = text };
  set('origin', location.origin);
  set('cookies', document.cookie);
  var url = (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws';
  var ws = new WebSocket(url);
  ws.onopen = function () { set('ws-status', 'open'); ws.send(${JSON.stringify(WS_GREETING)}) };
  ws.onmessage = function (e) { set('ws-echo', typeof e.data === 'string' ? e.data : 'binary:' + e.data.byteLength) };
  ws.onerror = function () { set('ws-status', 'error') };
  ws.onclose = function () { set('ws-status', 'closed') };
  // 用例从外面（frame.evaluate）驱动的几个动作
  window.e2e = {
    cookie: function () { return document.cookie },
    // 开一条新的 WebSocket，发 message，等回显（返回 { echo, protocol }）
    wsRoundTrip: function (message, protocols) {
      return new Promise(function (resolve, reject) {
        var s = new WebSocket(url, protocols);
        s.binaryType = 'arraybuffer';
        s.onopen = function () { s.send(message) };
        s.onmessage = function (e) {
          var echo = typeof e.data === 'string' ? e.data : new Uint8Array(e.data);
          var protocol = s.protocol;
          s.close(1000, 'done');
          resolve({ echo: typeof echo === 'string' ? echo : Array.from(echo), protocol: protocol });
        };
        s.onerror = function () { reject(new Error('websocket error')) };
      });
    },
  };
})();
`

const LANDED = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Landed</title>
</head>
<body>
<h1 id="heading">${LANDED_HEADING}</h1>
<p id="origin"></p>
<script>document.getElementById('origin').textContent = location.origin</script>
</body>
</html>
`

function readBody(req: IncomingMessage): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = []
    req.on('data', (c: Buffer) => chunks.push(c))
    req.on('end', () => resolve(Buffer.concat(chunks)))
    req.on('error', reject)
  })
}

function respond(
  req: IncomingMessage,
  res: ServerResponse,
  status: number,
  headers: Record<string, string | string[]>,
  body: string | Buffer,
  options: { gzip: boolean } = { gzip: false },
): void {
  let payload = typeof body === 'string' ? Buffer.from(body) : body
  const out: Record<string, string | string[] | number> = { ...headers }
  if (options.gzip && /\bgzip\b/.test(String(req.headers['accept-encoding'] ?? ''))) {
    payload = gzipSync(payload)
    out['content-encoding'] = 'gzip'
    out['vary'] = 'accept-encoding'
  }
  out['content-length'] = payload.length
  res.writeHead(status, out)
  res.end(req.method === 'HEAD' ? undefined : payload)
}

function json(req: IncomingMessage, res: ServerResponse, value: unknown): void {
  respond(
    req,
    res,
    200,
    { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' },
    JSON.stringify(value),
  )
}

async function handle(port: number, req: IncomingMessage, res: ServerResponse): Promise<void> {
  const url = new URL(req.url ?? '/', `http://localhost:${port}`)
  const html = { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' }
  const js = {
    'content-type': 'application/javascript; charset=utf-8',
    'cache-control': 'no-store',
  }
  switch (url.pathname) {
    case '/':
      return respond(req, res, 200, html, PAGE, { gzip: true })
    case '/app.js':
      return respond(req, res, 200, js, APP_JS)
    case '/big.js':
      return respond(req, res, 200, js, bigScript().source, { gzip: true })
    case '/big-identity.js':
      return respond(req, res, 200, js, bigScript().source)
    case '/api/set-cookie':
      return respond(
        req,
        res,
        200,
        {
          'content-type': 'text/plain; charset=utf-8',
          'cache-control': 'no-store',
          'set-cookie': [
            'e2e_plain=plain-value; Path=/',
            'e2e_secret=secret-value; Path=/; HttpOnly',
          ],
        },
        'cookies set\n',
      )
    case '/api/cookie':
      return json(req, res, { cookie: req.headers.cookie ?? '' })
    case '/api/headers':
      return json(req, res, { headers: req.headers })
    case '/api/echo': {
      const body = await readBody(req)
      return json(req, res, {
        method: req.method,
        search: url.search,
        contentType: req.headers['content-type'] ?? '',
        body: body.toString('utf8'),
      })
    }
    case '/redirect':
      return respond(
        req,
        res,
        302,
        { location: `http://localhost:${port}/landed`, 'cache-control': 'no-store' },
        '',
      )
    case '/landed':
      return respond(req, res, 200, html, LANDED)
    default:
      return respond(req, res, 404, { 'content-type': 'text/plain; charset=utf-8' }, 'not found\n')
  }
}

// ---------------------------------------------------------------- WebSocket 回显

const WS_GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'

function wsFrame(opcode: number, payload: Buffer): Buffer {
  const len = payload.length
  const head =
    len < 126
      ? Buffer.from([0x80 | opcode, len])
      : len < 65536
        ? Buffer.from([0x80 | opcode, 126, len >> 8, len & 0xff])
        : (() => {
            const b = Buffer.alloc(10)
            b[0] = 0x80 | opcode
            b[1] = 127
            b.writeBigUInt64BE(BigInt(len), 2)
            return b
          })()
  return Buffer.concat([head, payload])
}

function upgrade(req: IncomingMessage, socket: Socket): void {
  const key = req.headers['sec-websocket-key']
  const path = new URL(req.url ?? '/', 'http://x').pathname
  if (path !== '/ws' || typeof key !== 'string') {
    socket.end('HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n')
    return
  }
  const accept = createHash('sha1')
    .update(key + WS_GUID)
    .digest('base64')
  // 客户端提了子协议就选第一个，让用例能断言 `protocol` 原样回来
  const offered = String(req.headers['sec-websocket-protocol'] ?? '')
    .split(',')
    .map((p) => p.trim())
    .filter(Boolean)
  const lines = [
    'HTTP/1.1 101 Switching Protocols',
    'Upgrade: websocket',
    'Connection: Upgrade',
    `Sec-WebSocket-Accept: ${accept}`,
  ]
  if (offered[0]) lines.push(`Sec-WebSocket-Protocol: ${offered[0]}`)
  socket.write(`${lines.join('\r\n')}\r\n\r\n`)

  let buffer: Buffer = Buffer.alloc(0)
  socket.on('data', (chunk: Buffer) => {
    buffer = Buffer.concat([buffer, chunk])
    for (;;) {
      if (buffer.length < 2) return
      const opcode = buffer[0]! & 0x0f
      const masked = (buffer[1]! & 0x80) !== 0
      let len = buffer[1]! & 0x7f
      let offset = 2
      if (len === 126) {
        if (buffer.length < 4) return
        len = buffer.readUInt16BE(2)
        offset = 4
      } else if (len === 127) {
        if (buffer.length < 10) return
        len = Number(buffer.readBigUInt64BE(2))
        offset = 10
      }
      const maskLen = masked ? 4 : 0
      if (buffer.length < offset + maskLen + len) return
      const mask = masked ? buffer.subarray(offset, offset + 4) : undefined
      const payload = Buffer.from(buffer.subarray(offset + maskLen, offset + maskLen + len))
      buffer = buffer.subarray(offset + maskLen + len)
      if (mask) for (let i = 0; i < payload.length; i++) payload[i] = payload[i]! ^ mask[i % 4]!
      if (opcode === 0x1) {
        socket.write(wsFrame(0x1, Buffer.from(WS_ECHO_PREFIX + payload.toString('utf8'))))
      } else if (opcode === 0x2) {
        socket.write(wsFrame(0x2, payload))
      } else if (opcode === 0x9) {
        socket.write(wsFrame(0xa, payload))
      } else if (opcode === 0x8) {
        socket.write(wsFrame(0x8, payload.subarray(0, 2)))
        socket.end()
        return
      }
    }
  })
  socket.on('error', () => socket.destroy())
}

/** 起上游，返回实际端口。 */
export async function startUpstream(): Promise<UpstreamServer> {
  let port = 0
  const server: Server = createServer((req, res) => {
    handle(port, req, res).catch((err: unknown) => {
      if (!res.headersSent) res.writeHead(500, { 'content-type': 'text/plain' })
      res.end(`upstream fixture error: ${String(err)}\n`)
    })
  })
  server.on('upgrade', (req, socket) => upgrade(req, socket as Socket))
  // 升级成 WebSocket 的连接不归 `closeAllConnections` 管：自己记下全部连接，关闭时一起销毁。
  // 否则 `server.close()` 的回调等不到，teardown 卡在这里，进程在清理临时目录之前就静悄悄地退出了。
  const sockets = new Set<Socket>()
  server.on('connection', (socket: Socket) => {
    sockets.add(socket)
    socket.once('close', () => sockets.delete(socket))
  })
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => resolve())
  })
  const address = server.address()
  if (address === null || typeof address === 'string') throw new Error('upstream: no address')
  port = address.port
  return {
    port,
    close: () =>
      new Promise<void>((resolve) => {
        server.close(() => resolve())
        for (const socket of sockets) socket.destroy()
      }),
  }
}
