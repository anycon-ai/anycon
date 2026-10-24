import { describe, expect, it } from 'vitest'
import { previewPort } from '@/test/preview'
import { sortPorts } from './usePreviewPorts'

describe('sortPorts', () => {
  it('orders by port number', () => {
    expect(sortPorts([previewPort(8080), previewPort(80), previewPort(3000)]).map((p) => p.port)).toEqual([80, 3000, 8080])
  })

  it('lists a port once, keeping the entry that says more than “config”', () => {
    const out = sortPorts([previewPort(3000, { source: 'config', process: null }), previewPort(3000, { source: 'session', process: 'vite' })])
    expect(out).toHaveLength(1)
    expect(out[0]).toMatchObject({ source: 'session', process: 'vite' })
    expect(sortPorts([previewPort(3000, { source: 'session' }), previewPort(3000, { source: 'config' })])[0]!.source).toBe('session')
  })
})
