import { act, render, renderHook, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { resetDb } from '@/db'
import { installIntersectionObserver, type FakeIo } from '@/test/intersection'
import { FakeRpc } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { resetBlobCache, writeCached } from './cache'
import { resetDownloads } from './download'
import { useBlobUrl, useInView } from './hooks'

let io: FakeIo

beforeEach(async () => {
  await resetDb()
  resetBlobCache()
  resetDownloads()
  setRpc(new FakeRpc())
  io = installIntersectionObserver()
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => {
  setRpc(null)
  io.restore()
})

function Probe() {
  const [ref, seen] = useInView<HTMLDivElement>()
  return <div ref={ref} data-testid="probe" data-seen={seen} />
}

describe('useInView', () => {
  it('turns true when the element first comes near the viewport, stays true, and stops observing', () => {
    render(<Probe />)
    expect(screen.getByTestId('probe')).toHaveAttribute('data-seen', 'false')
    expect(io.observed).toHaveLength(1)
    act(() => io.intersect())
    expect(screen.getByTestId('probe')).toHaveAttribute('data-seen', 'true')
    expect(io.observed).toEqual([])
  })

  it('is true at once where IntersectionObserver does not exist', () => {
    io.restore()
    vi.stubGlobal('IntersectionObserver', undefined)
    render(<Probe />)
    expect(screen.getByTestId('probe')).toHaveAttribute('data-seen', 'true')
  })

  it('disconnects when the element goes away before it was seen', () => {
    const { unmount } = render(<Probe />)
    expect(io.observed).toHaveLength(1)
    unmount()
    expect(io.observed).toEqual([])
  })
})

describe('useBlobUrl', () => {
  it('does nothing while disabled', () => {
    const { result } = renderHook(() => useBlobUrl('n', 'a'.repeat(32), false))
    expect(result.current.status).toBe('idle')
  })

  it('goes loading → ready with a blob: URL for a cached image, and revokes it when the id changes', async () => {
    await writeCached('a'.repeat(32), new Uint8Array(4).fill(1), 'image/png')
    await writeCached('b'.repeat(32), new Uint8Array(4).fill(2), 'image/jpeg')
    const revoke = vi.spyOn(URL, 'revokeObjectURL')
    const { result, rerender } = renderHook(({ id }) => useBlobUrl('n', id, true), { initialProps: { id: 'a'.repeat(32) } })
    expect(result.current.status).toBe('loading')
    await vi.waitFor(() => expect(result.current.status).toBe('ready'))
    const first = result.current.url!
    expect(result.current.mime).toBe('image/png')
    rerender({ id: 'b'.repeat(32) })
    expect(revoke).toHaveBeenCalledWith(first)
    await vi.waitFor(() => expect(result.current.mime).toBe('image/jpeg'))
    expect(result.current.url).not.toBe(first)
  })

  it('ends in error (not a thrown rejection) when the image cannot be had', async () => {
    const { result } = renderHook(() => useBlobUrl('n', 'f'.repeat(32), true))
    await vi.waitFor(() => expect(result.current.status).toBe('error'))
    expect(result.current.url).toBeUndefined()
  })
})
