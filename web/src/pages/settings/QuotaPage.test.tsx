// /settings/quota（只有 multi）：GET /api/quota，每项 used / max，null 上限 = 不限；hub store 的 quota（quota_update）一变就重新拉。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { act, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ApiQuota } from '@/api/types'
import { useHub } from '@/stores/hub'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { mockRouter } from '@/test/routerMock'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

import { QuotaPage } from './QuotaPage'

const LIMITED: ApiQuota = { devices: 4, devices_max: 10, daemons: 3, daemons_max: 3, connections: 9, connections_max: 10, daily_pairings: 2, daily_pairings_max: 20 }

let quota: ApiQuota

function mount(reply?: () => { status: number; body: unknown }) {
  const api = installFakeApi({ 'GET /api/quota': reply ?? (() => ({ body: quota })) })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <QuotaPage />
    </QueryClientProvider>,
  )
  return api
}

beforeEach(() => {
  quota = { ...LIMITED }
  useHub.setState({ quota: null })
})
afterEach(() => vi.unstubAllGlobals())

describe('QuotaPage', () => {
  it('shows used / max for devices, nodes, live connections and pairings in the last 24 hours', async () => {
    mount()
    expect(await screen.findByTestId('quota-devices-value')).toHaveTextContent('4 of 10')
    expect(screen.getByTestId('quota-daemons-value')).toHaveTextContent('3 of 3')
    expect(screen.getByTestId('quota-connections-value')).toHaveTextContent('9 of 10')
    expect(screen.getByTestId('quota-dailyPairings-value')).toHaveTextContent('2 of 20')
  })

  it('marks a full limit and one that is nearly full, with words, not just color', async () => {
    mount()
    await screen.findByTestId('quota-devices-value')
    expect(screen.getByTestId('quota-daemons')).toHaveTextContent('Full')
    expect(screen.getByTestId('quota-connections')).toHaveTextContent('Almost full')
    expect(screen.getByTestId('quota-devices')).not.toHaveTextContent(/Full|Almost/)
  })

  it('a null max means unlimited: no bar, no badge', async () => {
    quota = { ...LIMITED, devices_max: null, daemons_max: null, connections_max: null, daily_pairings_max: null }
    mount()
    expect(await screen.findByTestId('quota-devices-value')).toHaveTextContent('4 · no limit')
    expect(screen.queryAllByRole('progressbar')).toHaveLength(0)
    expect(screen.getByTestId('quota-list')).not.toHaveTextContent(/Full|Almost/)
  })

  it('a bar per limited row, exposing the numbers to assistive tech', async () => {
    mount()
    await screen.findByTestId('quota-devices-value')
    const bars = screen.getAllByRole('progressbar')
    expect(bars).toHaveLength(4)
    expect(bars[0]).toHaveAttribute('aria-valuenow', '4')
    expect(bars[0]).toHaveAttribute('aria-valuemax', '10')
    expect(bars[0]).toHaveAccessibleName('Devices')
  })

  it('links to Settings → Devices, where room is freed', async () => {
    mount()
    expect(await screen.findByTestId('quota-devices-link')).toHaveAttribute('href', '/settings/devices')
  })

  it('a quota_update pushed over the connection (hub store) refreshes the numbers', async () => {
    const api = mount()
    expect(await screen.findByTestId('quota-devices-value')).toHaveTextContent('4 of 10')
    quota = { ...LIMITED, devices: 5, daily_pairings: 3 }
    act(() => useHub.getState().setQuota({ devices: 5, devices_max: 10, daemons: 3, daemons_max: 3, connections: 9, connections_max: 10 }))
    await waitFor(() => expect(screen.getByTestId('quota-devices-value')).toHaveTextContent('5 of 10'))
    expect(screen.getByTestId('quota-dailyPairings-value')).toHaveTextContent('3 of 20')
    expect(api.callsTo('GET /api/quota')).toHaveLength(2)
  })

  it('does not refetch just because the page mounted with a quota already in the store', async () => {
    useHub.setState({ quota: { devices: 4, devices_max: 10, daemons: 3, daemons_max: 3, connections: 9, connections_max: 10 } })
    const api = mount()
    await screen.findByTestId('quota-devices-value')
    await new Promise((r) => setTimeout(r, 50))
    expect(api.callsTo('GET /api/quota')).toHaveLength(1)
  })

  it('a failed request is reported', async () => {
    mount(() => ({ status: 500, body: errorBody('internal') }))
    expect(await screen.findByRole('alert')).toHaveTextContent('The hub had an error.')
  })
})
