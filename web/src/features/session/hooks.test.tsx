// useSessionSettings：上一次 session.set_settings 还在途中时，第二次改动要排队等它返回，不能被丢掉。
import { act, renderHook, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { SessionSettings } from '@/protocol'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { mockAgent, nodeEntry } from '@/test/factories'

const setSessionSettings = vi.fn()
vi.mock('./actions', () => ({ setSessionSettings: (...a: unknown[]) => setSessionSettings(...a) }))

import { useDeviceLabel, useSessionSettings } from './hooks'

const agent = mockAgent()

function deferred() {
  let resolve!: () => void
  const promise = new Promise<void>((r) => (resolve = r))
  return { promise, resolve }
}

function setup(baseSettings: SessionSettings = { model: 'mock-large', thinking: 'high', mode: 'ask' }) {
  return renderHook(
    (props: { running: boolean; offline: boolean; base: SessionSettings }) =>
      useSessionSettings({ nodeId: 'n1', sessionId: 's1', agent, running: props.running, baseSettings: props.base, offline: props.offline }),
    { initialProps: { running: false, offline: false, base: baseSettings } },
  )
}

beforeEach(() => {
  setSessionSettings.mockReset()
})

describe('useSessionSettings', () => {
  it('applies a change straight away when idle', async () => {
    setSessionSettings.mockResolvedValue(undefined)
    const { result } = setup()
    act(() => result.current.onChange({ ...result.current.selection!, thinking: 'low' }))
    await waitFor(() => expect(setSessionSettings).toHaveBeenCalledTimes(1))
    expect(setSessionSettings.mock.calls[0]![2]).toMatchObject({ settings: { thinking: 'low' } })
  })

  it('a second change while set_settings is still in flight is queued and sent after it returns (not dropped)', async () => {
    const first = deferred()
    setSessionSettings.mockImplementationOnce(() => first.promise).mockResolvedValue(undefined)
    const { result } = setup()

    act(() => result.current.onChange({ ...result.current.selection!, thinking: 'low' }))
    await waitFor(() => expect(setSessionSettings).toHaveBeenCalledTimes(1))

    // 第一个还没返回：用户又改了模式
    act(() => result.current.onChange({ ...result.current.selection!, mode: 'plan' }))
    expect(setSessionSettings).toHaveBeenCalledTimes(1) // 不会并发地发
    expect(result.current.queued).toBe(true)
    expect(result.current.selection?.mode).toBe('plan') // 控件立刻反映排队的改动

    await act(async () => {
      first.resolve()
      await first.promise
    })
    await waitFor(() => expect(setSessionSettings).toHaveBeenCalledTimes(2))
    expect(setSessionSettings.mock.calls[1]![2]).toMatchObject({ settings: { mode: 'plan' } })
    await waitFor(() => expect(result.current.queued).toBe(false))
  })

  it('several changes during one in-flight request are merged into a single follow-up update', async () => {
    const first = deferred()
    setSessionSettings.mockImplementationOnce(() => first.promise).mockResolvedValue(undefined)
    const { result } = setup()

    act(() => result.current.onChange({ ...result.current.selection!, thinking: 'low' }))
    await waitFor(() => expect(setSessionSettings).toHaveBeenCalledTimes(1))
    act(() => result.current.onChange({ ...result.current.selection!, mode: 'plan' }))
    act(() => result.current.onChange({ ...result.current.selection!, options: { verbose: true } }))

    await act(async () => {
      first.resolve()
      await first.promise
    })
    await waitFor(() => expect(setSessionSettings).toHaveBeenCalledTimes(2))
    expect(setSessionSettings.mock.calls[1]![2].settings).toMatchObject({ mode: 'plan', options: { verbose: true } })
  })

  it('still queues while the turn is running and applies when it ends', async () => {
    setSessionSettings.mockResolvedValue(undefined)
    const { result, rerender } = setup()
    rerender({ running: true, offline: false, base: { model: 'mock-large', thinking: 'high', mode: 'ask' } })
    act(() => result.current.onChange({ ...result.current.selection!, mode: 'plan' }))
    expect(setSessionSettings).not.toHaveBeenCalled()
    rerender({ running: false, offline: false, base: { model: 'mock-large', thinking: 'high', mode: 'ask' } })
    await waitFor(() => expect(setSessionSettings).toHaveBeenCalledTimes(1))
  })
})

describe('useDeviceLabel', () => {
  const NODE = 'n'.repeat(32)
  it('names this device “you”, the daemon “anycon”, known devices by name, and unknown ones by a short id', () => {
    useHub.setState({ snapshot: { ...useHub.getState().snapshot, deviceId: 'self-device-id' } })
    useNodes.setState({
      byId: { [NODE]: nodeEntry({ id: NODE, devices: [{ device_id: 'dev-iphone-0001', name: 'iPhone', kind: 'web' } as never] }) },
      order: [NODE],
      loaded: true,
    })
    const { result } = renderHook(() => useDeviceLabel(NODE))
    expect(result.current('self-device-id')).toBe('you')
    expect(result.current('daemon')).toBe('anycon') // 不是原始的 "daemon"
    expect(result.current('dev-iphone-0001')).toBe('iPhone')
    expect(result.current('abcdef0123456789')).toBe('abcdef01')
  })
})
