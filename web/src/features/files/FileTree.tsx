// 目录树（docs/spec/web.md § 4.7）：子目录第一次展开时才 `fs.list`，目录在前、按名字排序，键盘可用（WAI-ARIA tree）。
import { useEffect, useMemo, useRef, useState, type Dispatch, type KeyboardEvent, type SetStateAction } from 'react'
import { AlertTriangle, ChevronRight, File, FileImage, Folder, FolderOpen } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { cn } from '@/ui/cn'
import { Spinner } from '@/ui/Misc'
import { describeFsError } from './errors'
import type { DirCache } from './hooks'
import { chain, fileKind, isHidden, joinPath } from './paths'

export interface FileTreeProps {
  /** 树根（项目目录）。 */
  root: string
  /** 根的显示名。 */
  rootLabel: string
  /** URL 里的路径：高亮它，展开它上面的每一级。 */
  selected: string
  /** `selected` 是目录时它自己也展开（还不知道是文件还是目录时为 undefined）。 */
  selectedIsDir: boolean | undefined
  cache: DirCache
  /** 展开了哪些目录。状态在页面里（手机上树和预览是两屏，切到预览再回来要保持展开）。 */
  expanded: Set<string>
  setExpanded: Dispatch<SetStateAction<Set<string>>>
  showHidden: boolean
  onOpenFile: (path: string) => void
}

/** 展开状态：一开始只有树根展开。 */
export function useExpanded(root: string): [Set<string>, Dispatch<SetStateAction<Set<string>>>] {
  return useState<Set<string>>(() => new Set(root ? [root] : []))
}

type Row =
  | { kind: 'entry'; path: string; name: string; depth: number; isDir: boolean; expanded: boolean }
  | { kind: 'status'; path: string; depth: number; status: 'loading' | 'error' | 'empty'; error?: unknown }

const INDENT_PX = 14

export function FileTree({ root, rootLabel, selected, selectedIsDir, cache, expanded, setExpanded, showHidden, onOpenFile }: FileTreeProps) {
  const { t } = useTranslation()
  const { dirs, load } = cache
  const [focused, setFocused] = useState<string | null>(null)
  const treeRef = useRef<HTMLDivElement>(null)

  // 选中的路径上面的每一级（和它自己，如果是目录）要展开，这样 URL 指向的文件在树里看得到
  const onChain = useMemo(() => new Set(chain(root, selected)), [root, selected])
  useEffect(() => {
    const open = chain(root, selected).filter((p) => p !== selected || selectedIsDir === true)
    setExpanded((prev) => (open.every((p) => prev.has(p)) ? prev : new Set([...prev, ...open])))
  }, [root, selected, selectedIsDir, setExpanded])
  // 展开了、还没拉过的目录去拉
  useEffect(() => {
    for (const p of expanded) load(p)
  }, [expanded, load])

  const rows = useMemo(() => {
    const out: Row[] = []
    const walk = (path: string, depth: number): void => {
      const st = dirs[path]
      if (!st || (st.status === 'loading' && !st.entries)) return void out.push({ kind: 'status', path, depth, status: 'loading' })
      if (st.status === 'error') return void out.push({ kind: 'status', path, depth, status: 'error', error: st.error })
      const entries = (st.entries ?? []).filter((e) => showHidden || !isHidden(e.name) || onChain.has(joinPath(path, e.name)))
      if (entries.length === 0) return void out.push({ kind: 'status', path, depth, status: 'empty' })
      for (const e of entries) {
        const p = joinPath(path, e.name)
        const open = e.is_dir && expanded.has(p)
        out.push({ kind: 'entry', path: p, name: e.name, depth, isDir: e.is_dir, expanded: open })
        if (open) walk(p, depth + 1)
      }
    }
    const rootOpen = expanded.has(root)
    out.push({ kind: 'entry', path: root, name: rootLabel, depth: 0, isDir: true, expanded: rootOpen })
    if (rootOpen) walk(root, 1)
    return out
  }, [dirs, expanded, root, rootLabel, showHidden, onChain])

  const entries = rows.filter((r): r is Extract<Row, { kind: 'entry' }> => r.kind === 'entry')
  // 能 Tab 进来的只有一行：上次聚焦的，没有就选中的，再没有就第一行
  const tabStop = (focused && entries.some((r) => r.path === focused) ? focused : entries.some((r) => r.path === selected) ? selected : entries[0]?.path) ?? null

  const toggle = (path: string, open: boolean): void =>
    setExpanded((prev) => {
      const next = new Set(prev)
      if (open) next.add(path)
      else next.delete(path)
      return next
    })
  const focusRow = (path: string): void => {
    setFocused(path)
    for (const el of treeRef.current?.querySelectorAll<HTMLElement>('[role="treeitem"]') ?? []) {
      if (el.dataset.path === path) {
        el.focus()
        return
      }
    }
  }
  const activate = (r: Extract<Row, { kind: 'entry' }>): void => {
    if (r.isDir) toggle(r.path, !r.expanded)
    else onOpenFile(r.path)
  }
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>): void => {
    const target = (e.target as HTMLElement).closest<HTMLElement>('[role="treeitem"]')
    const i = entries.findIndex((r) => r.path === target?.dataset.path)
    const cur = entries[i]
    if (!cur || e.altKey || e.ctrlKey || e.metaKey) return
    const go = (j: number): void => {
      e.preventDefault()
      const r = entries[Math.max(0, Math.min(entries.length - 1, j))]
      if (r) focusRow(r.path)
    }
    switch (e.key) {
      case 'ArrowDown':
        return go(i + 1)
      case 'ArrowUp':
        return go(i - 1)
      case 'Home':
        return go(0)
      case 'End':
        return go(entries.length - 1)
      case 'ArrowRight':
        e.preventDefault()
        if (cur.isDir && !cur.expanded) toggle(cur.path, true)
        else if (cur.isDir && entries[i + 1]?.depth === cur.depth + 1) focusRow(entries[i + 1]!.path)
        return
      case 'ArrowLeft': {
        e.preventDefault()
        if (cur.isDir && cur.expanded) return toggle(cur.path, false)
        const parent = [...entries.slice(0, i)].reverse().find((r) => r.depth === cur.depth - 1)
        if (parent) focusRow(parent.path)
        return
      }
      case 'Enter':
      case ' ':
        e.preventDefault()
        return activate(cur)
    }
  }

  // 选中的行滚进视野（点面包屑、点 Markdown 里的链接、直接打开 URL 时）。每个选中的路径只滚一次：
  // 之后展开别的目录、行数变了，不能把用户正在看的位置拉回去
  const scrolledTo = useRef<string | null>(null)
  useEffect(() => {
    if (scrolledTo.current === selected) return
    for (const el of treeRef.current?.querySelectorAll<HTMLElement>('[role="treeitem"]') ?? []) {
      if (el.dataset.path === selected) {
        scrolledTo.current = selected
        el.scrollIntoView({ block: 'nearest' })
        return
      }
    }
  }, [selected, rows])

  return (
    <div ref={treeRef} role="tree" aria-label={t('files.tree')} className="py-1 text-sm" onKeyDown={onKeyDown} data-testid="files-tree">
      {rows.map((r) =>
        r.kind === 'entry' ? (
          <div
            key={r.path}
            role="treeitem"
            aria-level={r.depth + 1}
            aria-expanded={r.isDir ? r.expanded : undefined}
            aria-selected={r.path === selected}
            tabIndex={r.path === tabStop ? 0 : -1}
            data-path={r.path}
            data-testid="files-row"
            onClick={() => {
              setFocused(r.path)
              activate(r)
            }}
            onFocus={() => setFocused(r.path)}
            style={{ paddingLeft: 8 + r.depth * INDENT_PX }}
            className={cn(
              'flex min-h-8 cursor-pointer items-center gap-1.5 rounded-md py-1 pr-2 outline-none select-none hover:bg-bg-hover focus-visible:ring-2 focus-visible:ring-accent/60 md:min-h-7',
              r.path === selected ? 'bg-accent-subtle text-accent-text' : 'text-fg',
            )}
          >
            {r.isDir ? <ChevronRight className={cn('size-3.5 shrink-0 text-fg-subtle transition-transform', r.expanded && 'rotate-90')} aria-hidden /> : <span className="size-3.5 shrink-0" aria-hidden />}
            <RowIcon row={r} />
            <span className="min-w-0 truncate" title={r.name}>
              {r.name}
            </span>
          </div>
        ) : (
          <StatusRow key={`${r.path}\0${r.status}`} row={r} cache={cache} />
        ),
      )}
    </div>
  )
}

function RowIcon({ row }: { row: Extract<Row, { kind: 'entry' }> }) {
  const cls = 'size-4 shrink-0 text-fg-subtle'
  if (row.isDir) return row.expanded ? <FolderOpen className={cls} aria-hidden /> : <Folder className={cls} aria-hidden />
  return fileKind(row.name).type === 'image' ? <FileImage className={cls} aria-hidden /> : <File className={cls} aria-hidden />
}

function StatusRow({ row, cache }: { row: Extract<Row, { kind: 'status' }>; cache: DirCache }) {
  const { t } = useTranslation()
  const style = { paddingLeft: 8 + row.depth * INDENT_PX + 20 }
  if (row.status === 'loading')
    return (
      <div className="flex min-h-7 items-center gap-2 py-1 text-xs text-fg-subtle" style={style} role="status" aria-busy="true" data-testid="files-row-loading">
        <Spinner className="size-3.5" />
        {t('files.loading')}
      </div>
    )
  if (row.status === 'empty')
    return (
      <div className="min-h-7 py-1 text-xs text-fg-subtle" style={style} data-testid="files-row-empty">
        {t('files.emptyFolder')}
      </div>
    )
  return (
    <div className="flex min-h-7 flex-wrap items-center gap-x-2 py-1 pr-2 text-xs text-warning" style={style} role="alert" data-testid="files-row-error" data-path={row.path}>
      <AlertTriangle className="size-3.5 shrink-0" aria-hidden />
      <span className="min-w-0">{describeFsError(row.error, t)}</span>
      <button type="button" className="rounded px-1 font-medium underline hover:text-fg" onClick={() => cache.load(row.path)} data-testid="files-row-retry">
        {t('files.retry')}
      </button>
    </div>
  )
}
