import { describe, expect, it } from 'vitest'
import { mockAgent, nodeInfo } from '@/test/factories'
import { attachmentMimes } from './support'

const info = (blobs: boolean) => nodeInfo({ capabilities: { audit: true, blobs, fs: true, search: false, workspace: true } })
const agent = (attachments?: string[]) => mockAgent({ capabilities: { env: false, interject: false, interrupt: true, resume: true, rewind: false, structured_output: false, thinking_stream: true, usage: true, attachments } })

describe('attachmentMimes', () => {
  it('needs both: a node with blobs and an agent that takes attachments', () => {
    expect(attachmentMimes(info(true), agent(['image/png', 'image/jpeg']))).toEqual(['image/png', 'image/jpeg'])
    expect(attachmentMimes(info(false), agent(['image/png']))).toBeNull()
    expect(attachmentMimes(info(true), agent())).toBeNull()
    expect(attachmentMimes(info(true), agent([]))).toBeNull()
    expect(attachmentMimes(info(true), undefined)).toBeNull()
    expect(attachmentMimes(undefined, agent(['image/png']))).toBeNull()
  })

  it('only formats the node can take count (the agent list ∩ png / jpeg / gif / webp)', () => {
    expect(attachmentMimes(info(true), agent(['application/pdf', 'image/webp', 'image/bmp']))).toEqual(['image/webp'])
    expect(attachmentMimes(info(true), agent(['application/pdf']))).toBeNull()
  })

  it('an agent without any capabilities object takes nothing', () => {
    expect(attachmentMimes(info(true), { capabilities: undefined })).toBeNull()
  })
})
