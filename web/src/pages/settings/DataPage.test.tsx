// /settings/data：浏览器报告的用量、缓存大小、确认后清空缓存、请求持久化存储。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import * as db from '@/db'
import { resetBlobCache, writeCached } from '@/features/attachments/cache'
import { useDrafts } from '@/stores/drafts'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { ev } from '@/test/factories'
import { DataPage } from './DataPage'

function stubStorage(over: Partial<StorageManager> = {}) {
  const storage = {
    estimate: () => Promise.resolve({ usage: 90 * 1024 * 1024, quota: 100 * 1024 * 1024 }),
    persisted: () => Promise.resolve(false),
    persist: vi.fn().mockResolvedValue(false),
    ...over,
  }
  vi.stubGlobal('navigator', { ...navigator, storage })
  return storage
}

beforeEach(async () => {
  await db.resetDb()
  resetBlobCache()
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {}, items: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useUi.setState({ toasts: [] })
  useUi.getState().setStorageError(false)
})
afterEach(() => vi.unstubAllGlobals())

describe('DataPage', () => {
  it('shows what the browser reports, as text and as a bar', async () => {
    stubStorage()
    render(<DataPage />)
    expect(await screen.findByTestId('data-usage-value')).toHaveTextContent('90 MiB of about 100 MiB')
    const bar = screen.getByRole('progressbar')
    expect(bar).toHaveAccessibleName('Used by this site')
    expect(bar).toHaveAttribute('aria-valuenow', String(90 * 1024 * 1024))
  })

  it('says so when the browser does not report usage, and when it cannot be asked to keep data', async () => {
    vi.stubGlobal('navigator', { ...navigator, storage: undefined })
    render(<DataPage />)
    expect(await screen.findByTestId('data-usage')).toHaveTextContent('does not report')
    expect(screen.getByTestId('data-persist-state')).toHaveAttribute('data-state', 'unsupported')
    expect(screen.queryByTestId('data-persist-ask')).toBeNull()
  })

  it('clears the cache only after confirming, then shows the new numbers', async () => {
    stubStorage()
    const user = userEvent.setup()
    await db.putEvents('s1', [ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' }, { session_id: 's1', seq: 1 }), ev({ type: 'turn_done', turn_id: 1, reason: 'completed' }, { session_id: 's1', seq: 2 })])
    await writeCached('b1', new Uint8Array(2048), 'image/png')
    useUi.getState().setStorageError(true)
    render(<DataPage />)
    expect(await screen.findByTestId('data-cache-events')).toHaveTextContent('2 events')
    expect(screen.getByTestId('data-cache-attachments')).toHaveTextContent('1 file · 2.0 KiB')

    await user.click(screen.getByTestId('data-clear'))
    // 确认框说清楚留下什么
    expect(screen.getByRole('dialog')).toHaveTextContent(/device key, paired nodes, drafts, unsent messages/)
    expect(await db.countEvents()).toBe(2)
    await user.click(screen.getByTestId('data-clear-confirm'))

    await waitFor(() => expect(screen.getByTestId('data-cache-events')).toHaveTextContent('0 events'))
    expect(screen.getByTestId('data-cache-attachments')).toHaveTextContent('0 files · 0 B')
    expect(screen.queryByRole('dialog')).toBeNull()
    expect(useUi.getState().storageError).toBe(false)
    expect(useUi.getState().toasts.at(-1)?.text).toBe('Cache cleared.')
  })

  it('asks the browser for persistent storage and explains a refusal', async () => {
    const storage = stubStorage()
    const user = userEvent.setup()
    render(<DataPage />)
    expect(await screen.findByTestId('data-persist-state')).toHaveAttribute('data-state', 'best_effort')
    expect(screen.getByTestId('data-persist')).toHaveTextContent(/including the device key/)
    await user.click(screen.getByTestId('data-persist-ask'))
    expect(storage.persist).toHaveBeenCalledOnce()
    expect(await screen.findByTestId('data-persist-denied')).toHaveTextContent('The browser said no')
  })

  it('shows it as on once the browser agrees', async () => {
    stubStorage({ persist: vi.fn().mockResolvedValue(true) })
    const user = userEvent.setup()
    render(<DataPage />)
    await user.click(await screen.findByTestId('data-persist-ask'))
    await waitFor(() => expect(screen.getByTestId('data-persist-state')).toHaveAttribute('data-state', 'persisted'))
    expect(screen.queryByTestId('data-persist-ask')).toBeNull()
    expect(screen.queryByTestId('data-persist-denied')).toBeNull()
  })
})
