import { useEffect, useState } from 'react'
import { RotateCcw, Trash2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { loadTrash, purgeTrash, restoreTrash } from '@/features/workspace/actions'
import { describeError } from '@/lib/errors'
import { formatBytes, formatRelative } from '@/lib/format'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Card, SectionTitle } from '@/ui/Misc'
import { WorktreeSection } from './WorktreeSection'

export function WorkspaceTab({ nodeId }: { nodeId: string }) {
  const { t, i18n } = useTranslation()
  const entry = useNodes((s) => s.byId[nodeId])
  const offline = entry?.conn !== 'ready'
  const [busy, setBusy] = useState<string | null>(null)

  useEffect(() => {
    if (!offline)
      loadTrash(nodeId).catch((e: unknown) => {
        console.warn('anycon: could not load the trash', e)
        toast.error(describeError(e, t))
      })
  }, [nodeId, offline, t])

  if (!entry) return null
  const run = async (id: string, f: () => Promise<void>): Promise<void> => {
    setBusy(id)
    try {
      await f()
    } catch (e) {
      toast.error(describeError(e, t))
    } finally {
      setBusy(null)
    }
  }
  const total = entry.trash.reduce((n, e) => n + e.bytes, 0)

  return (
    <div className="space-y-6">
      <Card className="divide-y divide-border">
        <Row label={t('nodeSettings.wsRoot')}>
          <span className="break-all font-mono text-xs">{entry.workspaceRoot ?? entry.info?.workspace_root ?? '—'}</span>
        </Row>
        <Row label={t('nodeSettings.wsProjects')}>{entry.projects.length}</Row>
      </Card>
      <section>
        <SectionTitle
          action={
            entry.trash.length > 0 ? (
              <Button
                size="sm"
                variant="outline-danger"
                disabled={offline || busy !== null}
                onClick={() => void run('all', async () => { for (const e of entry.trash) await purgeTrash(nodeId, e.id) })}
                data-testid="trash-empty"
              >
                {t('nodeSettings.emptyTrash')}
              </Button>
            ) : null
          }
        >
          {t('nodeSettings.trash')} {entry.trash.length > 0 ? `· ${formatBytes(total)}` : ''}
        </SectionTitle>
        {entry.trash.length === 0 ? (
          <p className="text-sm text-fg-subtle">{t('nodeSettings.trashEmpty')}</p>
        ) : (
          <ul className="space-y-2">
            {entry.trash.map((e) => (
              <li key={e.id}>
                <Card className="flex items-center gap-3 p-3">
                  <div className="min-w-0 flex-1">
                    <p className="truncate font-medium">{e.original_name}</p>
                    <p className="text-xs text-fg-subtle">
                      {formatBytes(e.bytes)} · {formatRelative(e.trashed_at_ms, i18n.language)}
                    </p>
                  </div>
                  <Button size="sm" variant="outline" icon={<RotateCcw className="size-4" />} disabled={offline || busy !== null} loading={busy === e.id} onClick={() => void run(e.id, () => restoreTrash(nodeId, e.id))}>
                    {t('nodeSettings.restore')}
                  </Button>
                  <Button size="sm" variant="ghost" className="text-danger" aria-label={t('nodeSettings.purge')} icon={<Trash2 className="size-4" />} disabled={offline || busy !== null} onClick={() => void run(e.id, () => purgeTrash(nodeId, e.id))} />
                </Card>
              </li>
            ))}
          </ul>
        )}
      </section>
      <WorktreeSection nodeId={nodeId} />
    </div>
  )
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 px-4 py-3 text-sm">
      <span className="text-fg-muted">{label}</span>
      <span className="min-w-0 text-right">{children}</span>
    </div>
  )
}
