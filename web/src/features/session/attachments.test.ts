// 用户消息里的图片附件（ADR-35）：reducer 把 `user_message.attachments` 放进 user block；导出写成 `[image: <name>]`。
import { beforeEach, describe, expect, it } from 'vitest'
import type { AttachmentRef } from '@/protocol'
import i18n from '@/i18n'
import { ev, evs, resetSeq } from '@/test/factories'
import { exportMarkdown } from './export'
import { applyEvents, buildView, emptyView, type UserBlock } from './reducer'

beforeEach(() => resetSeq())

const shot: AttachmentRef = { blob_id: 'a'.repeat(32), name: 'shot.png', mime: 'image/png', bytes: 120_000, width: 1280, height: 800 }
const photo: AttachmentRef = { blob_id: 'b'.repeat(32), name: '照片 1.jpg', mime: 'image/jpeg', bytes: 3_000_000, width: null, height: null }

describe('reducer: user_message attachments', () => {
  it('puts the daemon-recorded attachments (with width and height) on the user block', () => {
    const v = buildView(evs([{ type: 'user_message', message_id: 'm1', content: 'look', by_device: 'me', attachments: [shot, photo] }]))
    expect((v.blocks[0] as UserBlock).attachments).toEqual([shot, photo])
  })

  it('a message without attachments has an empty list', () => {
    const v = buildView(evs([{ type: 'user_message', message_id: 'm1', content: 'hi', by_device: 'me' }]))
    expect((v.blocks[0] as UserBlock).attachments).toEqual([])
  })

  it('an image-only message is a user block with empty content', () => {
    const v = buildView(evs([{ type: 'user_message', message_id: 'm1', content: '', by_device: 'me', attachments: [shot] }]))
    expect(v.blocks[0]).toMatchObject({ type: 'user', content: '', attachments: [shot] })
  })

  it('the duplicate of an already-seen message does not change its attachments', () => {
    const e = ev({ type: 'user_message', message_id: 'm1', content: 'hi', by_device: 'd', attachments: [shot] })
    const v = applyEvents(applyEvents(emptyView(), [e]), [{ ...e, seq: e.seq + 1 }])
    expect(v.blocks.filter((b) => b.type === 'user')).toHaveLength(1)
    expect((v.blocks[0] as UserBlock).attachments).toEqual([shot])
  })
})

describe('export', () => {
  const blocks = () =>
    buildView(
      evs([
        { type: 'user_message', message_id: 'm1', content: 'look at these', by_device: 'me', attachments: [shot, photo] },
        { type: 'user_message', message_id: 'm2', content: '', by_device: 'me', attachments: [shot] },
        { type: 'user_message', message_id: 'm3', content: 'plain', by_device: 'me' },
      ]),
    ).blocks

  it('lists each attachment as [image: name] after the text, and nothing is embedded', () => {
    const md = exportMarkdown('T', blocks())
    expect(md).toContain('## User\n\nlook at these\n\n[image: shot.png]\n\n[image: 照片 1.jpg]\n')
    expect(md).not.toContain(shot.blob_id)
  })

  it('an image-only message has only the image lines', () => {
    const md = exportMarkdown('T', blocks())
    expect(md).toContain('## User\n\n[image: shot.png]\n\n## User')
  })

  it('a message without images exports exactly as before', () => {
    expect(exportMarkdown('T', blocks())).toContain('## User\n\nplain\n')
  })

  it('follows the UI language through the label callback', async () => {
    await i18n.changeLanguage('zh')
    try {
      const md = exportMarkdown('T', blocks(), (name) => i18n.t('export.image', { name }))
      expect(md).toContain('[图片：shot.png]')
      expect(md).not.toContain('[image:')
      await i18n.changeLanguage('en')
      expect(exportMarkdown('T', blocks(), (name) => i18n.t('export.image', { name }))).toContain('[image: shot.png]')
    } finally {
      await i18n.changeLanguage('en')
    }
  })
})
