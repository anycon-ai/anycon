/**
 * 图片附件（ADR-35）：真实 hub + daemon（mock agent）+ web。
 *
 *   a. 选图（输入框里的「＋」菜单 →「添加文件或照片」→ 文件选择框）→ 缩略图上传完成 → 带文字发送：mock 回复里的 `Attachments: … sha256 …` 和本地算的一致
 *      （证明 daemon 把正确的文件交给了 agent）；用户气泡里有缩略图；只发图片不带文字也行；粘贴一张图
 *   b. 清掉 IndexedDB 的图片缓存后刷新、以及另一台全新设备打开同一个会话：历史里的图经 `blob.get` 拉回来；
 *      点开缩略图看大图；hub 日志里没有文件名
 *
 * mock agent 的回显见 crates/adapters/src/mock/README.md（`Attachments:` 那一行）。
 * 约定：显式等待（data-testid），没有固定睡眠。每个用例自己配对自己的浏览器。
 */
import { createHash } from 'node:crypto'
import { deflateSync } from 'node:zlib'

import { expect, test, type Locator, type Page } from '@playwright/test'

import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  TURN_TIMEOUT,
  expectNodeOnline,
  hubLogLeaks,
  nodeBase,
  pairNewBrowser,
} from '../helpers/ui.js'

test.beforeEach(() => {
  test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
})

// ---- 生成测试图片（不依赖第三方包）----

const CRC_TABLE = (() => {
  const t = new Uint32Array(256)
  for (let n = 0; n < 256; n++) {
    let c = n
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
    t[n] = c >>> 0
  }
  return t
})()

function crc32(buf: Buffer): number {
  let c = 0xffffffff
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff]! ^ (c >>> 8)
  return (c ^ 0xffffffff) >>> 0
}

function chunk(type: string, data: Buffer): Buffer {
  const len = Buffer.alloc(4)
  len.writeUInt32BE(data.length)
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data])
  const crc = Buffer.alloc(4)
  crc.writeUInt32BE(crc32(body))
  return Buffer.concat([len, body, crc])
}

/** 纯色 RGB PNG。 */
function solidPng(width: number, height: number, rgb: [number, number, number]): Buffer {
  const ihdr = Buffer.alloc(13)
  ihdr.writeUInt32BE(width, 0)
  ihdr.writeUInt32BE(height, 4)
  ihdr[8] = 8 // bit depth
  ihdr[9] = 2 // truecolor
  const row = Buffer.alloc(1 + width * 3)
  for (let x = 0; x < width; x++) row.set(rgb, 1 + x * 3)
  const raw = Buffer.concat(Array.from({ length: height }, () => row))
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw)),
    chunk('IEND', Buffer.alloc(0)),
  ])
}

/** mock 回显里这张图的那一段：`<name> (image/png, <bytes> bytes, sha256 <前 12 位>)`。 */
function echoOf(name: string, bytes: Buffer): string {
  const sha = createHash('sha256').update(bytes).digest('hex').slice(0, 12)
  return `${name} (image/png, ${bytes.length} bytes, sha256 ${sha})`
}

const RED = solidPng(48, 32, [220, 30, 30])
const BLUE = solidPng(40, 40, [30, 60, 220])
const GREEN = solidPng(24, 36, [30, 200, 60])

// ---- 页面操作 ----

/** 经「＋」菜单选图：打开菜单、点「添加文件或照片」，由真正弹出的文件选择框交文件。 */
async function attachViaMenu(page: Page, name: string, bytes: Buffer): Promise<void> {
  await page.getByTestId('composer-plus').click()
  const [chooser] = await Promise.all([
    page.waitForEvent('filechooser'),
    page.getByTestId('composer-attach').click(),
  ])
  await chooser.setFiles({ name, mimeType: 'image/png', buffer: bytes })
}

/** 直接往隐藏的 `<input type=file>` 交文件（不经菜单）。 */
async function attachFile(page: Page, name: string, bytes: Buffer): Promise<void> {
  await page
    .getByTestId('composer-file-input')
    .setInputFiles({ name, mimeType: 'image/png', buffer: bytes })
}

/** 在输入框里粘贴一张图（构造带文件的 ClipboardEvent）。 */
async function pasteImage(page: Page, name: string, bytes: Buffer): Promise<void> {
  await page.getByTestId('composer-input').evaluate(
    (el, { fileName, data }) => {
      const dt = new DataTransfer()
      dt.items.add(new File([new Uint8Array(data)], fileName, { type: 'image/png' }))
      el.dispatchEvent(
        new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }),
      )
    },
    { fileName: name, data: Array.from(bytes) },
  )
}

async function expectAttachmentsReady(page: Page, count: number): Promise<void> {
  const items = page.getByTestId('composer-attachment')
  await expect(items).toHaveCount(count)
  for (let i = 0; i < count; i++) {
    await expect(items.nth(i)).toHaveAttribute('data-state', 'ready', { timeout: CONNECT_TIMEOUT })
  }
}

async function waitIdle(page: Page): Promise<void> {
  await expect(page.getByTestId('session-status')).toHaveAttribute('data-status', 'idle', {
    timeout: TURN_TIMEOUT,
  })
}

/** 缩略图真的画出来了（`blob:` 地址、解码成功）。 */
async function expectImageLoaded(thumb: Locator): Promise<void> {
  // 历史里的缩略图进入视口才拉（IntersectionObserver）
  await thumb.scrollIntoViewIfNeeded()
  await expect(thumb).toHaveAttribute('data-state', 'ready', { timeout: CONNECT_TIMEOUT })
  const img = thumb.locator('img')
  await expect(img).toHaveAttribute('src', /^blob:/, { timeout: CONNECT_TIMEOUT })
  await expect
    .poll(
      () =>
        img.evaluate(
          (el) => (el as HTMLImageElement).complete && (el as HTMLImageElement).naturalWidth,
        ),
      {
        timeout: CONNECT_TIMEOUT,
      },
    )
    .toBeGreaterThan(0)
}

/** 清空这个浏览器 IndexedDB 里的图片缓存。 */
async function clearBlobCache(page: Page): Promise<void> {
  await page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const req = indexedDB.open('anycon')
      req.onsuccess = () => resolve(req.result)
      req.onerror = () => reject(req.error)
    })
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction('blobs', 'readwrite')
      tx.objectStore('blobs').clear()
      tx.oncomplete = () => resolve()
      tx.onerror = () => reject(tx.error)
    })
    db.close()
  })
}

test.describe('图片附件', () => {
  test('a + b. 选图 / 只发图片 / 粘贴 → agent 收到原文件；刷新、新设备都能看到历史里的图', async ({
    browser,
  }) => {
    test.setTimeout(180_000)
    const state = readState()
    const d1 = await pairNewBrowser(browser, state, { deviceName: 'e2e-attach-1' })
    const d2 = await pairNewBrowser(browser, state, { deviceName: 'e2e-attach-2' })
    try {
      const page = d1.page
      // 1. 新建 scratch 会话：先选图，上传完成后带文字发送
      await page.goto(`${nodeBase(page)}/new`)
      await expect(page.getByTestId('composer-plus')).toBeVisible({ timeout: CONNECT_TIMEOUT })
      await attachViaMenu(page, 'red.png', RED)
      await expectAttachmentsReady(page, 1)
      // 点缩略图看大图（和消息里的图共用同一个查看器）；Esc 关掉之后缩略图还在
      await page.getByTestId('composer-attachment-open').click()
      await expect(page.getByTestId('attachment-viewer')).toContainText('red.png')
      await page.keyboard.press('Escape')
      await expect(page.getByTestId('attachment-viewer')).toHaveCount(0)
      await expectAttachmentsReady(page, 1)
      await page.getByTestId('composer-input').fill('what is in this picture')
      await page.getByTestId('composer-send').click()
      await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
      const sessionPath = new URL(page.url()).pathname

      const replies = page.getByTestId('message-assistant')
      await expect(replies.last()).toContainText(`Attachments: ${echoOf('red.png', RED)}`, {
        timeout: TURN_TIMEOUT,
      })
      await expect(replies.last()).toContainText('You said: what is in this picture')
      const users = page.getByTestId('message-user')
      await expect(users.first().getByTestId('message-attachment')).toHaveCount(1)
      await expectImageLoaded(users.first().getByTestId('message-attachment').first())
      // 发完之后输入区的附件清空
      await expect(page.getByTestId('composer-attachment')).toHaveCount(0)
      await waitIdle(page)

      // 2. 只发图片，不带文字
      await attachFile(page, 'blue.png', BLUE)
      await expectAttachmentsReady(page, 1)
      await expect(page.getByTestId('composer-send')).toBeEnabled()
      await page.getByTestId('composer-send').click()
      await expect(replies).toHaveCount(2, { timeout: TURN_TIMEOUT })
      await expect(replies.last()).toContainText(`Attachments: ${echoOf('blue.png', BLUE)}`, {
        timeout: TURN_TIMEOUT,
      })
      await waitIdle(page)

      // 3. 粘贴一张图 + 再选一张：一条消息两个附件，顺序保持
      await pasteImage(page, 'green.png', GREEN)
      await expectAttachmentsReady(page, 1)
      await attachFile(page, 'red-again.png', RED)
      await expectAttachmentsReady(page, 2)
      await page.getByTestId('composer-input').fill('two pictures')
      await page.getByTestId('composer-send').click()
      await expect(replies).toHaveCount(3, { timeout: TURN_TIMEOUT })
      await expect(replies.last()).toContainText(
        `Attachments: ${echoOf('green.png', GREEN)}; ${echoOf('red-again.png', RED)}`,
        { timeout: TURN_TIMEOUT },
      )
      await waitIdle(page)
      await expect(page.getByTestId('message-attachment')).toHaveCount(4)

      // 4. 清掉图片缓存再刷新：历史里的图经 blob.get 重新拉回来
      await clearBlobCache(page)
      await page.reload()
      await expectNodeOnline(page)
      const thumbs = page.getByTestId('message-attachment')
      await expect(thumbs).toHaveCount(4, { timeout: CONNECT_TIMEOUT })
      for (let i = 0; i < 4; i++) await expectImageLoaded(thumbs.nth(i))
      await expect(page.getByTestId('message-attachment-error')).toHaveCount(0)

      // 5. 点开看大图
      await thumbs.first().click()
      const viewer = page.getByTestId('attachment-viewer')
      await expect(viewer).toBeVisible()
      await expect(viewer).toContainText('red.png')
      await page.keyboard.press('Escape')
      await expect(viewer).toBeHidden()

      // 6. 全新设备（空缓存）打开同一个会话
      await d2.page.goto(sessionPath)
      await expectNodeOnline(d2.page)
      const thumbs2 = d2.page.getByTestId('message-attachment')
      await expect(thumbs2).toHaveCount(4, { timeout: CONNECT_TIMEOUT })
      for (let i = 0; i < 4; i++) await expectImageLoaded(thumbs2.nth(i))

      // 7. hub 只转发密文：日志里没有文件名
      expect(hubLogLeaks(state, ['red.png', 'blue.png', 'green.png', 'red-again.png'])).toEqual([])
    } finally {
      await d2.context.close()
      await d1.context.close()
    }
  })
})
