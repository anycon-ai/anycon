import { Fragment, useEffect, useId, useMemo, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { FolderGit2, FolderOpen, FolderPlus, GitFork, MessageSquare, MessageSquareText, Plus, Server } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useDevices } from '@/api/queries'
import { useContentSearch, type ContentHit } from '@/features/search/contentSearch'
import { lockedNodes } from '@/features/unlock/nodes'
import { formatDateTime, formatRelative } from '@/lib/format'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { useCurrentNode } from '@/lib/nav'
import { useNodeFilter } from '@/lib/nodeFilter'
import { useNewProject, type NewProjectMode } from '@/stores/newProject'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { cn } from '@/ui/cn'
import { Input } from '@/ui/Field'
import { Spinner } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

interface Item {
  id: string
  icon: React.ReactNode
  label: string
  hint?: string
  run: () => void
}

/** 一条会话内容的命中：选中后打开那个会话并定位到 `seq` 那条消息（spec/web.md § 9）。 */
interface HitItem {
  id: string
  hit: ContentHit
  run: () => void
}

/** 命中片段：只有 `<mark>` 里的是高亮，其余都是文本节点（片段来自会话内容，不能当 HTML，见 features/search/snippet.ts）。 */
function SnippetText({ parts }: { parts: ContentHit['snippet'] }) {
  return (
    <>
      {parts.map((p, i) =>
        p.mark ? (
          <mark key={i} className="rounded-sm bg-warning-subtle px-0.5 font-medium text-fg">
            {p.text}
          </mark>
        ) : (
          <Fragment key={i}>{p.text}</Fragment>
        ),
      )}
    </>
  )
}

/**
 * ⌘K：按标题搜本地已知的会话、项目、节点，执行「新建会话」「新建项目」「克隆仓库」「关联已有目录」；
 * 输入够长（≥ 2 个字符）时还对左栏勾选的在线节点做会话内容的全文搜索（daemon 的 `session.search`），命中单独成一组。
 * 键盘：↑ ↓ 在所有结果里移动，Enter 打开选中的那条（默认第一条），Esc 关闭。
 */
export function CommandPalette({ open, onOpenChange }: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const { t, i18n } = useTranslation()
  const navigate = useNavigate()
  const { nodeId } = useCurrentNode()
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  const sessions = useSessions((s) => s.list)
  const { selected } = useNodeFilter()
  const devices = useDevices()
  const [q, setQ] = useState('')
  const [activeId, setActiveId] = useState<string | null>(null)
  const idBase = useId()
  const search = useContentSearch(q, open)

  // 关掉（点了某一项、Esc、再按一次 ⌘K）之后下次从空白开始：不留着旧的查询去重新搜一遍
  useEffect(() => {
    if (!open) {
      setQ('')
      setActiveId(null)
    }
  }, [open])

  const items = useMemo<Item[]>(() => {
    const out: Item[] = []
    const go = (fn: () => void) => () => {
      onOpenChange(false)
      setQ('')
      fn()
    }
    if (nodeId) {
      const param = shortNodeId(nodeId, order)
      out.push({ id: 'new', icon: <Plus className="size-4" />, label: t('palette.newSession'), run: go(() => void navigate({ to: '/n/$node/new', params: { node: param } })) })
      const newProject = (mode: NewProjectMode) => go(() => useNewProject.getState().open({ nodeId, mode, then: 'project', pickNode: true }))
      out.push({ id: 'new-project', icon: <FolderPlus className="size-4" />, label: t('project.new'), run: newProject('empty') })
      out.push({ id: 'clone-project', icon: <GitFork className="size-4" />, label: t('project.newClone'), run: newProject('clone') })
      out.push({ id: 'link-project', icon: <FolderOpen className="size-4" />, label: t('project.openExisting'), run: newProject('existing') })
      for (const p of nodes[nodeId]?.projects ?? [])
        out.push({ id: `p-${p.name}`, icon: <FolderGit2 className="size-4" />, label: p.name, hint: t('palette.project'), run: go(() => void navigate({ to: '/n/$node/p/$project', params: { node: param, project: p.name } })) })
    }
    for (const id of order) {
      const n = nodes[id]
      if (n) out.push({ id: `n-${id}`, icon: <Server className="size-4" />, label: n.name, hint: t('palette.node'), run: go(() => void navigate({ to: '/n/$node', params: { node: shortNodeId(id, order) } })) })
    }
    for (const s of Object.values(sessions).sort((a, b) => b.updated_at_ms - a.updated_at_ms)) {
      out.push({
        id: `s-${s.id}`,
        icon: <MessageSquare className="size-4" />,
        label: s.title || s.id,
        hint: s.project ?? t('palette.scratch'),
        run: go(() => void navigate({ to: '/n/$node/s/$session', params: { node: shortNodeId(s.node_id, order), session: s.id } })),
      })
    }
    return out
  }, [nodeId, nodes, order, sessions, navigate, onOpenChange, t])

  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase()
    return (needle ? items.filter((i) => i.label.toLowerCase().includes(needle) || i.hint?.toLowerCase().includes(needle)) : items).slice(0, 30)
  }, [items, q])

  const hitItems = useMemo<HitItem[]>(
    () =>
      search.hits.map((hit) => ({
        id: `h-${hit.key}`,
        hit,
        run: () => {
          onOpenChange(false)
          setQ('')
          void navigate({ to: '/n/$node/s/$session', params: { node: shortNodeId(hit.nodeId, order), session: hit.sessionId }, search: { seq: hit.seq } })
        },
      })),
    [search.hits, order, navigate, onOpenChange],
  )

  // ↑ ↓ 在本地结果和内容命中之间移动；还没动过（或选中的那条没了）就是第一条
  const selectable = useMemo<Array<{ id: string; run: () => void }>>(() => [...filtered, ...hitItems], [filtered, hitItems])
  const active = selectable.find((r) => r.id === activeId) ?? selectable[0]
  const domId = (id: string): string => `${idBase}-${id}`

  const nodeName = (id: string): string => cleanHintText(nodes[id]?.name) || id.slice(0, 8)
  const list = new Intl.ListFormat(i18n.language, { style: 'long', type: 'conjunction' })
  // 没搜的节点：勾选了但不在线的，加上账号下这个浏览器还没解锁的（它们不在勾选里，也搜不了）
  const notSearched = [...search.skipped.map(nodeName), ...lockedNodes(devices.data, order).map((n) => cleanHintText(n.name) || n.id.slice(0, 8))]
  const succeeded = search.searched - search.failed.length
  const tooShort = [...q.trim()].length === 1

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>): void => {
    // 输入法选词时的 ↑ ↓ Enter 属于输入法
    if (e.nativeEvent.isComposing) return
    if ((e.key === 'ArrowDown' || e.key === 'ArrowUp') && selectable.length > 0) {
      e.preventDefault()
      const at = Math.max(0, selectable.findIndex((r) => r.id === active?.id))
      const next = selectable[(at + (e.key === 'ArrowDown' ? 1 : selectable.length - 1)) % selectable.length]!
      setActiveId(next.id)
      document.getElementById(domId(next.id))?.scrollIntoView({ block: 'nearest' })
    } else if (e.key === 'Enter') {
      active?.run()
    }
  }

  const rowClass = (id: string): string => cn('flex w-full rounded-md px-2 py-2 text-left text-sm', active?.id === id && 'bg-bg-hover')
  const rowProps = (id: string) => ({ id: domId(id), 'aria-current': active?.id === id ? ('true' as const) : undefined, onMouseMove: () => setActiveId(id) })

  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={t('palette.title')} hideTitle closeLabel={t('common.close')} size="lg" placement="top">
      <Input
        autoFocus
        autoComplete="off"
        value={q}
        onChange={(e) => {
          setQ(e.target.value)
          setActiveId(null)
        }}
        placeholder={t('palette.placeholder')}
        aria-label={t('palette.placeholder')}
        aria-controls={`${idBase}-list`}
        aria-activedescendant={active ? domId(active.id) : undefined}
        onKeyDown={onKeyDown}
      />
      <ul id={`${idBase}-list`} className="scroll-thin mt-3 max-h-[min(26rem,60dvh)] space-y-0.5 overflow-y-auto">
        {filtered.map((i) => (
          <li key={i.id}>
            <button type="button" onClick={i.run} className={cn(rowClass(i.id), 'items-center gap-2')} {...rowProps(i.id)}>
              <span className="text-fg-subtle">{i.icon}</span>
              <span className="min-w-0 flex-1 truncate">{i.label}</span>
              {i.hint ? <span className="shrink-0 text-xs text-fg-subtle">{i.hint}</span> : null}
            </button>
          </li>
        ))}
        {filtered.length === 0 && !search.active ? <li className="px-2 py-6 text-center text-sm text-fg-subtle">{t('palette.empty')}</li> : null}
        {search.active ? (
          <>
            <li role="presentation" className="px-2 pb-1 pt-3 text-xs font-medium text-fg-subtle" data-testid="palette-content-group">
              {t('palette.content')}
            </li>
            {hitItems.map(({ id, hit, run }) => (
              <li key={id}>
                <button type="button" onClick={run} className={cn(rowClass(id), 'flex-col gap-0.5')} data-testid="palette-hit" {...rowProps(id)}>
                  <span className="flex w-full items-center gap-2">
                    <MessageSquareText className="size-4 shrink-0 text-fg-subtle" aria-hidden />
                    <span className="min-w-0 flex-1 truncate font-medium">{hit.title || hit.sessionId}</span>
                    {selected.length > 1 ? (
                      <span className="max-w-[35%] shrink-0 truncate text-xs text-fg-subtle" data-testid="palette-hit-node">
                        {nodeName(hit.nodeId)}
                      </span>
                    ) : null}
                    <time className="shrink-0 text-xs text-fg-subtle" dateTime={new Date(hit.tsMs).toISOString()} title={formatDateTime(hit.tsMs, i18n.language)}>
                      {formatRelative(hit.tsMs, i18n.language)}
                    </time>
                  </span>
                  <span className="line-clamp-2 w-full break-words pl-6 text-xs text-fg-muted" data-testid="palette-hit-snippet">
                    <SnippetText parts={hit.snippet} />
                  </span>
                </button>
              </li>
            ))}
            {search.searching ? (
              <li role="status" className="flex items-center gap-2 px-2 py-2 text-xs text-fg-subtle" data-testid="palette-searching">
                <Spinner className="size-3.5" />
                {t('palette.searching')}
              </li>
            ) : succeeded > 0 && search.hits.length === 0 ? (
              <li role="status" className="px-2 py-2 text-xs text-fg-subtle" data-testid="palette-no-content-match">
                {t('palette.noContentMatch')}
              </li>
            ) : null}
            {!search.searching && search.failed.length > 0 ? (
              <li role="status" className="px-2 py-1 text-xs text-danger" data-testid="palette-failed">
                {t('palette.failed', { names: list.format(search.failed.map(nodeName)) })}
              </li>
            ) : null}
            {notSearched.length > 0 ? (
              <li className="px-2 py-1 text-xs text-fg-subtle" data-testid="palette-skipped">
                {t('palette.skipped', { names: list.format(notSearched) })}
              </li>
            ) : null}
          </>
        ) : tooShort ? (
          <li className="px-2 py-2 text-xs text-fg-subtle" data-testid="palette-min-chars">
            {t('palette.minChars')}
          </li>
        ) : null}
      </ul>
    </Dialog>
  )
}
