/**
 * 最小的 SMTP 服务（Node `net`），给 hub 的邮箱验证码 / 通知邮件用（ADR-37）：hub 配 `ANYCON_SMTP_URL=smtp+plain://127.0.0.1:<port>`。
 *
 * 说的话和 hub 的 Rust 版测试服务（`crates/hub/tests/common/smtp.rs`）一样：lettre 对 `smtp+plain://` 不开 TLS、没写凭据就不发 AUTH，
 * 一个连接上依次是 `220` → `EHLO`（多行 250，不宣告 STARTTLS / AUTH）→ `MAIL FROM` → `RCPT TO` → `DATA`（354，读到单独一行 `.`，
 * 行首的 `..` 还原成 `.`）→ `250` → `QUIT`（221）。每封信一个连接。
 *
 * 服务在 globalSetup 的进程里，而用例在 worker 进程里，所以收到的信**落成文件**（`<dir>/<序号>.json`，先写临时文件再改名，读的人不会读到半截）：
 * 用例用下面的 `readMails` / `waitForCode` 读目录，不需要跟服务进程通信。
 */
import { createServer, type Server, type Socket } from 'node:net'
import { mkdirSync, readdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

export interface ReceivedMail {
  /** 收到的顺序，从 1 开始。 */
  seq: number
  mailFrom: string
  /** 信封收件人（小写，hub 规范化过）。 */
  rcptTo: string[]
  /** DATA 的内容：头 + 空行 + 正文，行尾 CRLF，点填充已经还原。 */
  data: string
}

export interface SmtpServer {
  port: number
  dir: string
  close: () => Promise<void>
}

/** `MAIL FROM:<a@b>` / `RCPT TO:<a@b> ...` 里尖括号中的地址。 */
function bracketed(line: string): string {
  const m = /<([^>]*)>/.exec(line)
  return m?.[1] ?? ''
}

function serve(socket: Socket, deliver: (mail: Omit<ReceivedMail, 'seq'>) => void): void {
  socket.setEncoding('utf8')
  const reply = (text: string): void => void socket.write(`${text}\r\n`)
  let buffer = ''
  let inData = false
  let mailFrom = ''
  let rcptTo: string[] = []
  let data: string[] = []
  reply('220 e2e.smtp ESMTP ready')
  socket.on('data', (chunk: string) => {
    buffer += chunk
    for (;;) {
      const eol = buffer.indexOf('\r\n')
      if (eol < 0) return
      const line = buffer.slice(0, eol)
      buffer = buffer.slice(eol + 2)
      if (inData) {
        if (line === '.') {
          inData = false
          deliver({ mailFrom, rcptTo, data: data.length ? `${data.join('\r\n')}\r\n` : '' })
          mailFrom = ''
          rcptTo = []
          data = []
          reply('250 2.0.0 queued')
        } else {
          data.push(line.startsWith('..') ? line.slice(1) : line)
        }
        continue
      }
      switch ((line.split(/\s+/)[0] ?? '').toUpperCase()) {
        case 'EHLO':
          socket.write('250-e2e.smtp\r\n250-8BITMIME\r\n250 SIZE 10485760\r\n')
          break
        case 'HELO':
          reply('250 e2e.smtp')
          break
        case 'MAIL':
          mailFrom = bracketed(line)
          rcptTo = []
          reply('250 ok')
          break
        case 'RCPT':
          rcptTo.push(bracketed(line).toLowerCase())
          reply('250 ok')
          break
        case 'DATA':
          inData = true
          data = []
          reply('354 end with <CRLF>.<CRLF>')
          break
        case 'RSET':
        case 'NOOP':
          reply('250 ok')
          break
        case 'QUIT':
          reply('221 bye')
          socket.end()
          return
        default:
          reply('500 unrecognised command')
      }
    }
  })
  socket.on('error', () => undefined)
}

/** 起服务（随机空闲端口），收到的信写进 `dir`。 */
export async function startSmtp(dir: string): Promise<SmtpServer> {
  mkdirSync(dir, { recursive: true })
  let seq = 0
  const sockets = new Set<Socket>()
  const server: Server = createServer((socket) => {
    sockets.add(socket)
    socket.on('close', () => sockets.delete(socket))
    serve(socket, (mail) => {
      seq += 1
      const name = String(seq).padStart(6, '0')
      const record: ReceivedMail = { seq, ...mail }
      writeFileSync(join(dir, `${name}.tmp`), JSON.stringify(record))
      renameSync(join(dir, `${name}.tmp`), join(dir, `${name}.json`))
    })
  })
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => resolve())
  })
  const address = server.address()
  if (!address || typeof address === 'string') throw new Error('the e2e SMTP server has no port')
  return {
    port: address.port,
    dir,
    close: () =>
      new Promise<void>((resolve) => {
        for (const socket of sockets) socket.destroy()
        server.close(() => resolve())
      }),
  }
}

// ---------------------------------------------------------------- 读（用例一侧）

/** 目录里收到的全部信，按收到的顺序。 */
export function readMails(dir: string): ReceivedMail[] {
  return readdirSync(dir)
    .filter((name) => name.endsWith('.json'))
    .toSorted()
    .map((name) => JSON.parse(readFileSync(join(dir, name), 'utf8')) as ReceivedMail)
}

/** 发给某个地址的信（信封收件人，大小写不拘）。 */
export function mailsTo(dir: string, address: string): ReceivedMail[] {
  const want = address.trim().toLowerCase()
  return readMails(dir).filter((m) => m.rcptTo.includes(want))
}

function decodeQuotedPrintable(text: string): string {
  const joined = text.replace(/=\r?\n/g, '')
  const bytes: number[] = []
  for (let i = 0; i < joined.length; i++) {
    const hex = joined.slice(i + 1, i + 3)
    if (joined[i] === '=' && /^[0-9A-Fa-f]{2}$/.test(hex)) {
      bytes.push(Number.parseInt(hex, 16))
      i += 2
    } else {
      bytes.push(...Buffer.from(joined[i] ?? '', 'utf8'))
    }
  }
  return Buffer.from(bytes).toString('utf8')
}

/** 头（DATA 里第一个空行之前）里某个头的值，大小写不拘，不处理折行；没有返回 undefined。 */
export function mailHeader(mail: ReceivedMail, name: string): string | undefined {
  const head = mail.data.split('\r\n\r\n')[0] ?? ''
  const prefix = `${name.toLowerCase()}:`
  const line = head.split('\r\n').find((l) => l.toLowerCase().startsWith(prefix))
  return line?.slice(prefix.length).trim()
}

/** 解开传输编码之后的正文（英文邮件是 7bit，中文邮件是 base64 / quoted-printable）。 */
export function mailText(mail: ReceivedMail): string {
  const body = mail.data.split('\r\n\r\n').slice(1).join('\r\n\r\n')
  switch ((mailHeader(mail, 'content-transfer-encoding') ?? '7bit').toLowerCase()) {
    case 'base64':
      return Buffer.from(body.replace(/\s+/g, ''), 'base64').toString('utf8')
    case 'quoted-printable':
      return decodeQuotedPrintable(body)
    default:
      return body
  }
}

/** 正文里的 6 位验证码（邮件里写成 `123 456`）。 */
export function codeOf(mail: ReceivedMail): string {
  const m = /(\d{3}) (\d{3})/.exec(mailText(mail))
  if (!m) throw new Error(`no verification code in the mail:\n${mailText(mail)}`)
  return `${m[1]}${m[2]}`
}

/**
 * 等到发给 `address` 的信多于 `after` 封，返回**最新**那封的验证码。
 * `after` 是发码之前 `mailsTo(...).length` 的值：登录和绑定各发一封信、码互不通用，靠数量区分哪封是刚发的。
 */
export async function waitForCode(
  dir: string,
  address: string,
  options: { after: number; timeoutMs?: number },
): Promise<string> {
  const deadline = Date.now() + (options.timeoutMs ?? 15_000)
  for (;;) {
    const got = mailsTo(dir, address)
    if (got.length > options.after) return codeOf(got[got.length - 1] as ReceivedMail)
    if (Date.now() > deadline) {
      throw new Error(
        `no mail for ${address} after ${options.after} (got ${got.length}) within the time limit`,
      )
    }
    await new Promise((resolve) => setTimeout(resolve, 100))
  }
}
