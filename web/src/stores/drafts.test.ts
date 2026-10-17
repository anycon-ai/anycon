import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import * as db from '@/db'
import { useUi } from '@/stores/ui'
import { resetDrafts, useDrafts } from './drafts'

beforeEach(async () => {
  await db.resetDb()
  resetDrafts()
  useUi.setState({ storageError: false })
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => vi.useRealTimers())

describe('draft persistence', () => {
  it('saves a draft shortly after typing', async () => {
    useDrafts.getState().setText('s1', 'hello')
    await vi.waitFor(async () => expect((await db.getDraft('s1'))?.text).toBe('hello'))
    expect(useUi.getState().storageError).toBe(false)
  })

  it('does not swallow a failed save: it warns and raises the storage-error flag for the status banner', async () => {
    vi.spyOn(db, 'putDraft').mockRejectedValue(new Error('QuotaExceededError'))
    useDrafts.getState().setText('s1', 'hello')
    await vi.waitFor(() => expect(useUi.getState().storageError).toBe(true))
    expect(console.warn).toHaveBeenCalledWith(expect.stringContaining('save the draft'), expect.any(Error))
  })
})
