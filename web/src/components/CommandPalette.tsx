import { useMemo, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { FolderGit2, FolderOpen, FolderPlus, GitFork, MessageSquare, Plus, Server } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useCurrentNode } from '@/lib/nav'
import { shortNodeId } from '@/lib/ids'
import { useNewProject, type NewProjectMode } from '@/stores/newProject'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { Dialog } from '@/ui/Overlay'
import { Input } from '@/ui/Field'

interface Item {
  id: string
  icon: React.ReactNode
  label: string
  hint?: string
  run: () => void
}

/** ⌘K：按标题搜本地已知的会话、项目、节点，或执行「新建会话」「新建项目」「克隆仓库」「关联已有目录」。（不是 daemon 的全文搜索。） */
export function CommandPalette({ open, onOpenChange }: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { nodeId } = useCurrentNode()
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  const sessions = useSessions((s) => s.list)
  const [q, setQ] = useState('')

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

  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={t('palette.title')} hideTitle closeLabel={t('common.close')}>
      <Input autoFocus value={q} onChange={(e) => setQ(e.target.value)} placeholder={t('palette.placeholder')} aria-label={t('palette.placeholder')} onKeyDown={(e) => { if (e.key === 'Enter') filtered[0]?.run() }} />
      <ul className="scroll-thin mt-3 max-h-72 space-y-0.5 overflow-y-auto">
        {filtered.map((i) => (
          <li key={i.id}>
            <button type="button" onClick={i.run} className="flex w-full items-center gap-2 rounded-md px-2 py-2 text-left text-sm hover:bg-bg-hover">
              <span className="text-fg-subtle">{i.icon}</span>
              <span className="min-w-0 flex-1 truncate">{i.label}</span>
              {i.hint ? <span className="shrink-0 text-xs text-fg-subtle">{i.hint}</span> : null}
            </button>
          </li>
        ))}
        {filtered.length === 0 ? <li className="px-2 py-6 text-center text-sm text-fg-subtle">{t('palette.empty')}</li> : null}
      </ul>
    </Dialog>
  )
}
