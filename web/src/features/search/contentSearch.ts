// 命令面板的会话内容搜索（spec/web.md § 9）：对左栏勾选的、在线的节点并发发 `session.search`（daemon 本地的 FTS5），
// 把各节点的命中合起来。搜索发生在浏览器和 daemon 之间（Noise 通道），hub 看不到查询和结果。
import { useEffect, useMemo, useState } from 'react'
import type { SessionSearchParams } from '@/protocol'
import { isRecord } from '@/lib/guards'
import { cleanHintText } from '@/lib/hint'
import { useNodeFilter } from '@/lib/nodeFilter'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { rpc } from '@/transport/instance'
import { parseSnippet, type SnippetPart } from './snippet'

/** 查询（去掉首尾空白）至少这么多个字符才搜：一个字符的前缀匹配会命中几乎所有消息。 */
export const MIN_QUERY_CHARS = 2
/** 停手这么久再发请求。 */
export const SEARCH_DEBOUNCE_MS = 300
/** 每个节点最多要几条命中（daemon 默认 20、上限 200）。 */
export const SEARCH_LIMIT_PER_NODE = 20
/** 搜索比一般请求更不值得等：10 秒没回就算这个节点失败，不拖住「正在搜索」。 */
export const SEARCH_TIMEOUT_MS = 10_000
const TITLE_MAX_CHARS = 200

export interface ContentHit {
  /** 列表 key：节点 + 会话 + seq。 */
  key: string
  nodeId: string
  sessionId: string
  /** 命中那段文字开始处的事件 seq（docs/protocol.md § 5.4）。 */
  seq: number
  title: string
  snippet: SnippetPart[]
  tsMs: number
}

/** 查询按字符（码点）算够不够长；返回去掉首尾空白的查询，不够长返回 ''。 */
export function normalizeQuery(raw: string): string {
  const q = raw.trim()
  return [...q].length >= MIN_QUERY_CHARS ? q : ''
}

/**
 * 校验并整理一个节点的 `session.search` 结果。节点是对端，回的东西不能照单全收：形状不对的命中丢掉，
 * 会话已经属于别的节点的命中也丢掉（和会话列表一样，节点只能说自己的会话，见 stores/sessions.ts）。
 * 整个结果不是 `{ hits: [...] }` 就抛错，算这个节点失败。
 */
export function parseHits(nodeId: string, result: unknown): ContentHit[] {
  if (!isRecord(result) || !Array.isArray(result.hits)) throw new Error('malformed session.search result')
  const known = useSessions.getState().list
  const seen = new Set<string>()
  const hits: ContentHit[] = []
  for (const h of result.hits as unknown[]) {
    if (!isRecord(h)) continue
    const { session_id: sessionId, seq, title, snippet, ts_ms: tsMs } = h
    if (typeof sessionId !== 'string' || sessionId === '' || typeof snippet !== 'string') continue
    if (typeof seq !== 'number' || !Number.isSafeInteger(seq) || seq < 0) continue
    if (typeof tsMs !== 'number' || !Number.isFinite(tsMs)) continue
    const owner = known[sessionId]?.node_id
    if (owner && owner !== nodeId) continue
    const key = `${nodeId}:${sessionId}:${seq}`
    if (seen.has(key)) continue
    seen.add(key)
    hits.push({ key, nodeId, sessionId, seq, title: typeof title === 'string' ? cleanHintText(title, TITLE_MAX_CHARS) : '', snippet: parseSnippet(snippet), tsMs })
  }
  return hits
}

export async function searchNode(nodeId: string, query: string, limit: number = SEARCH_LIMIT_PER_NODE): Promise<ContentHit[]> {
  const params: SessionSearchParams = { query, limit }
  return parseHits(nodeId, await rpc().request<unknown>(nodeId, 'session.search', params, SEARCH_TIMEOUT_MS))
}

export interface ContentSearch {
  /** 查询够长：显示「会话内容」这一组。 */
  active: boolean
  /** 还有节点没回，或者输入还没停下来（防抖中）。 */
  searching: boolean
  /** 各节点的命中，按勾选的顺序、节点内按 daemon 给的顺序。 */
  hits: ContentHit[]
  /** 这次搜了、但出错的节点（别的节点的结果照常显示）。 */
  failed: string[]
  /** 勾选了、但不在线所以没搜的节点。 */
  skipped: string[]
  /** 这次要搜的节点数；0 = 没有一个勾选的节点在线。 */
  searched: number
}

type NodeResult = { hits: ContentHit[] } | { error: true }

/**
 * `rawQuery` 是输入框里的原文；`enabled` 是面板开着。查询停手 300 ms 后，对勾选的在线节点并发搜索，
 * 先回来的先显示。查询或节点集合变了就作废所有还没回的请求（迟到的响应不会写进新一轮的结果）。
 */
export function useContentSearch(rawQuery: string, enabled: boolean): ContentSearch {
  const { selected } = useNodeFilter()
  const readyKey = useNodes((s) => selected.filter((id) => s.byId[id]?.conn === 'ready').join(','))
  const searchable = useMemo(() => (readyKey ? readyKey.split(',') : []), [readyKey])
  const query = enabled ? normalizeQuery(rawQuery) : ''

  const [debounced, setDebounced] = useState('')
  useEffect(() => {
    if (query === '') {
      setDebounced('')
      return
    }
    const timer = setTimeout(() => setDebounced(query), SEARCH_DEBOUNCE_MS)
    return () => clearTimeout(timer)
  }, [query])

  // key 把「搜的是什么、搜哪些节点」绑在一起：结果只在 key 对得上时才算当前的
  const key = `${readyKey}\n${debounced}`
  const [state, setState] = useState<{ key: string; byNode: Record<string, NodeResult> }>({ key: '', byNode: {} })
  useEffect(() => {
    if (debounced === '') return
    let stale = false
    setState({ key, byNode: {} })
    const settle = (nodeId: string, result: NodeResult): void => {
      if (!stale) setState((s) => (s.key === key ? { key, byNode: { ...s.byNode, [nodeId]: result } } : s))
    }
    for (const nodeId of searchable) {
      searchNode(nodeId, debounced).then(
        (hits) => settle(nodeId, { hits }),
        (e: unknown) => {
          console.warn(`anycon: session search failed on node ${nodeId.slice(0, 8)}`, e)
          settle(nodeId, { error: true })
        },
      )
    }
    return () => {
      stale = true
    }
  }, [debounced, key, searchable])

  const active = query !== ''
  const current = debounced === query && state.key === key
  const byNode = current ? state.byNode : {}
  const hits: ContentHit[] = []
  const failed: string[] = []
  let waiting = false
  for (const id of searchable) {
    const r = byNode[id]
    if (!r) waiting = true
    else if ('error' in r) failed.push(id)
    else hits.push(...r.hits)
  }
  return {
    active,
    searching: active && (!current || waiting),
    hits: active && current ? hits : [],
    failed: active && current ? failed : [],
    skipped: selected.filter((id) => !searchable.includes(id)),
    searched: searchable.length,
  }
}
