// 预览入口什么时候出现（web.md § 13.1）：hub 配了预览（模板不为空）并且节点 capabilities.preview = true。
import type { ReactNode } from 'react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { renderHook } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { keys } from '@/api/queries'
import type { ApiConfig } from '@/api/types'
import { nodeInfo } from '@/test/factories'
import { PREVIEW_TEMPLATE, previewNodeInfo } from '@/test/preview'
import { previewAvailable, usePreviewTemplate } from './availability'

function templateFor(config: Partial<ApiConfig>): string | null {
  const client = new QueryClient()
  client.setQueryData(keys.config, { mode: 'self-hosted', login_methods: ['token'], protocol_version: 1, version: '0', ...config })
  const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>
  return renderHook(() => usePreviewTemplate(), { wrapper }).result.current
}

describe('usePreviewTemplate', () => {
  it('is the hub’s preview_origin_template when it has one', () => {
    expect(templateFor({ preview_origin_template: PREVIEW_TEMPLATE })).toBe(PREVIEW_TEMPLATE)
  })

  it.each([
    ['null (preview not configured)', null],
    ['missing (an older hub)', undefined],
    ['empty', ''],
    ['without {label}', 'https://preview.example'],
  ])('is null when the template is %s', (_name, template) => {
    expect(templateFor({ preview_origin_template: template })).toBeNull()
  })
})

describe('previewAvailable', () => {
  it('needs both the hub’s template and a node with capabilities.preview', () => {
    expect(previewAvailable(PREVIEW_TEMPLATE, previewNodeInfo())).toBe(true)
    expect(previewAvailable(null, previewNodeInfo())).toBe(false)
    expect(previewAvailable(PREVIEW_TEMPLATE, nodeInfo())).toBe(false)
    expect(previewAvailable(PREVIEW_TEMPLATE, undefined)).toBe(false)
    expect(previewAvailable(PREVIEW_TEMPLATE, nodeInfo({ capabilities: { ...nodeInfo().capabilities, preview: false } }))).toBe(false)
  })
})
