// 草稿里的图片附件（ADR-35）：传完的引用跟着草稿持久化，上传中 / 失败的不存。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AttachmentRef } from '@/protocol'
import * as db from '@/db'
import { onDraftsReset, resetDrafts, useDrafts, type AttachmentItem } from './drafts'

const ref = (n: number): AttachmentRef => ({ blob_id: String(n).repeat(32), name: `p${n}.png`, mime: 'image/png', bytes: 10 * n, width: 4, height: 3 })
const item = (n: number, over: Partial<AttachmentItem> = {}): AttachmentItem => ({ id: `i${n}`, name: `p${n}.png`, mime: 'image/png', state: 'ready', progress: 1, ref: ref(n), ...over })

beforeEach(async () => {
  await db.resetDb()
  resetDrafts()
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => vi.useRealTimers())

describe('attachments in the draft', () => {
  it('mirrors only the finished references, in list order, and saves them', async () => {
    const d = useDrafts.getState()
    d.updateItems('s1', () => [item(1), item(2, { state: 'uploading', progress: 0.4, ref: undefined }), item(3), item(4, { state: 'error', ref: undefined })])
    expect(useDrafts.getState().byKey['s1']!.attachments?.map((r) => r.name)).toEqual(['p1.png', 'p3.png'])
    await vi.waitFor(async () => expect((await db.getDraft('s1'))?.attachments?.map((r) => r.name)).toEqual(['p1.png', 'p3.png']))
  })

  it('does not rewrite the draft when only progress changes', () => {
    const d = useDrafts.getState()
    d.updateItems('s1', () => [item(1), item(2, { state: 'uploading', progress: 0, ref: undefined })])
    const before = useDrafts.getState().byKey['s1']
    d.updateItems('s1', (l) => l.map((i) => (i.id === 'i2' ? { ...i, progress: 0.5 } : i)))
    expect(useDrafts.getState().byKey['s1']).toBe(before)
  })

  it('a draft that has only attachments (no text) is kept; once nothing is left it is deleted', async () => {
    const d = useDrafts.getState()
    d.updateItems('s1', () => [item(1)])
    await vi.waitFor(async () => expect(await db.getDraft('s1')).toBeDefined())
    d.updateItems('s1', () => [])
    await vi.waitFor(async () => expect(await db.getDraft('s1')).toBeUndefined())
  })

  it('hydrateItems fills an untouched list once, and never overwrites a list that exists (even an empty one after sending)', () => {
    const d = useDrafts.getState()
    d.hydrateItems('s1', [item(1)])
    d.hydrateItems('s1', [item(2)])
    expect(useDrafts.getState().items['s1']!.map((i) => i.id)).toEqual(['i1'])
    d.updateItems('s1', () => [])
    d.hydrateItems('s1', [item(3)])
    expect(useDrafts.getState().items['s1']).toEqual([])
    // 恢复列表不碰草稿本身
    expect(useDrafts.getState().byKey['s2']).toBeUndefined()
    d.hydrateItems('s2', [item(5)])
    expect(useDrafts.getState().byKey['s2']).toBeUndefined()
  })

  it('load brings the saved attachments back; what is already in memory wins', async () => {
    await db.putDraft({ session_id: 's1', text: 'saved', attachments: [ref(1)], unsent: [] })
    await useDrafts.getState().load('s1')
    expect(useDrafts.getState().byKey['s1']).toMatchObject({ text: 'saved', attachments: [ref(1)] })

    resetDrafts()
    await db.putDraft({ session_id: 's2', text: '', attachments: [ref(1)], unsent: [] })
    useDrafts.getState().updateItems('s2', () => [item(2)])
    await useDrafts.getState().load('s2')
    expect(useDrafts.getState().byKey['s2']!.attachments).toEqual([ref(2)])
  })

  it('loadAll restores the attachments of drafts in sessions that were never opened', async () => {
    await db.putDraft({ session_id: 'old', text: '', attachments: [ref(7)], unsent: [] })
    await useDrafts.getState().loadAll()
    expect(useDrafts.getState().byKey['old']!.attachments).toEqual([ref(7)])
  })

  it('keeps the attachments of unsent messages on disk', async () => {
    useDrafts.getState().addUnsent('s1', { message_id: 'm1', content: '', created_at: 1, state: 'unsent', attachments: [ref(1), ref(2)] })
    await vi.waitFor(async () => expect((await db.getDraft('s1'))?.unsent[0]?.attachments).toEqual([ref(1), ref(2)]))
  })
})

describe('resetDrafts', () => {
  it('revokes the local previews, clears the lists and runs the registered clean-ups', () => {
    const revoke = vi.spyOn(URL, 'revokeObjectURL')
    const cleanup = vi.fn()
    const off = onDraftsReset(cleanup)
    useDrafts.getState().updateItems('s1', () => [item(1, { previewUrl: 'blob:test/a' }), item(2)])
    resetDrafts()
    expect(revoke).toHaveBeenCalledWith('blob:test/a')
    expect(cleanup).toHaveBeenCalledTimes(1)
    expect(useDrafts.getState().items).toEqual({})
    expect(useDrafts.getState().byKey).toEqual({})
    off()
    resetDrafts()
    expect(cleanup).toHaveBeenCalledTimes(1)
  })
})
