// `session.search` 的结果来自节点（对端）：形状不对的丢掉，别的节点的会话不认，片段和标题按文本处理。
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { summary } from '@/test/factories'
import { setRpc, type NodeRpc } from '@/transport/instance'
import { useSessions } from '@/stores/sessions'
import { normalizeQuery, parseHits, searchNode, SEARCH_LIMIT_PER_NODE, SEARCH_TIMEOUT_MS } from './contentSearch'

const NODE = 'n'.repeat(32)
const OTHER = 'o'.repeat(32)

const raw = (over: Record<string, unknown> = {}) => ({ session_id: 'sess_1', seq: 7, title: 'T', snippet: 'a <mark>b</mark>', ts_ms: 1000, ...over })

beforeEach(() => useSessions.setState({ list: {}, pending: {}, open: {} }))
afterEach(() => setRpc(null))

describe('normalizeQuery', () => {
  it('trims, and needs at least 2 characters (code points)', () => {
    expect(normalizeQuery('  ab ')).toBe('ab')
    expect(normalizeQuery('a b')).toBe('a b')
    expect(normalizeQuery(' a ')).toBe('')
    expect(normalizeQuery('')).toBe('')
    expect(normalizeQuery('库')).toBe('')
    expect(normalizeQuery('数据')).toBe('数据')
    expect(normalizeQuery('😀')).toBe('')
  })
})

describe('parseHits', () => {
  it('keeps well-formed hits with a parsed snippet', () => {
    const hits = parseHits(NODE, { hits: [raw(), raw({ session_id: 'sess_2', seq: 0, ts_ms: 5 })] })
    expect(hits).toHaveLength(2)
    expect(hits[0]).toEqual({ key: `${NODE}:sess_1:7`, nodeId: NODE, sessionId: 'sess_1', seq: 7, title: 'T', snippet: [{ text: 'a ', mark: false }, { text: 'b', mark: true }], tsMs: 1000 })
  })

  it('drops hits with the wrong shape, and duplicates', () => {
    const hits = parseHits(NODE, {
      hits: [raw(), raw(), null, 'x', 5, [], raw({ session_id: '' }), raw({ session_id: 3 }), raw({ seq: -1 }), raw({ seq: 1.5 }), raw({ seq: '7' }), raw({ seq: Number.MAX_SAFE_INTEGER + 1 }), raw({ snippet: 1 }), raw({ ts_ms: 'x' }), raw({ ts_ms: NaN })],
    })
    expect(hits.map((h) => h.key)).toEqual([`${NODE}:sess_1:7`])
  })

  it('rejects a result that is not a hit list (the node counts as failed)', () => {
    for (const bad of [undefined, null, 'x', [], {}, { hits: 'x' }, { hits: {} }]) expect(() => parseHits(NODE, bad)).toThrow(/malformed/)
  })

  it('ignores hits on sessions this browser already knows to belong to another node', () => {
    useSessions.getState().upsertSummaries(OTHER, [summary({ id: 'theirs' })])
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'mine' })])
    const hits = parseHits(NODE, { hits: [raw({ session_id: 'theirs' }), raw({ session_id: 'mine' }), raw({ session_id: 'unknown_yet' })] })
    expect(hits.map((h) => h.sessionId)).toEqual(['mine', 'unknown_yet'])
  })

  it('cleans control and bidi characters out of the title, and a missing title is empty (the palette falls back to the id)', () => {
    expect(parseHits(NODE, { hits: [raw({ title: 'a‮b\nc' })] })[0]!.title).toBe('a b c')
    expect(parseHits(NODE, { hits: [raw({ title: undefined })] })[0]!.title).toBe('')
  })
})

describe('searchNode', () => {
  it('asks the node for a bounded number of hits with a short timeout', async () => {
    const calls: unknown[][] = []
    const rpc: NodeRpc = {
      isReady: () => true,
      request: async (...args) => {
        calls.push(args)
        return { hits: [raw()] } as never
      },
    }
    setRpc(rpc)
    const hits = await searchNode(NODE, 'parser')
    expect(hits).toHaveLength(1)
    expect(calls).toEqual([[NODE, 'session.search', { query: 'parser', limit: SEARCH_LIMIT_PER_NODE }, SEARCH_TIMEOUT_MS]])
  })
})
