// 分身页的「使用别的节点」：列出这个节点的分身用着的节点，添加（两步 RPC）和断开。
// 短码只在 `addTwinLink` 里经内存传递；这里的状态和渲染里都没有它。
import { useCallback, useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { TwinLink } from '@/protocol'
import { describeError } from '@/lib/errors'
import { shortNodeId } from '@/lib/ids'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Chip, Dot } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'
import { addTwinLink, candidateBlock, describeLinkError, groupsDiffer, isUnsupported, listTwinLinks, removeTwinLink } from './links'

export function TwinLinks({ node }: { node: NodeEntry }) {
  const { t } = useTranslation()
  const [links, setLinks] = useState<TwinLink[] | null>(null)
  const [unsupported, setUnsupported] = useState(false)
  const [failed, setFailed] = useState(false)
  const [adding, setAdding] = useState(false)
  const [removing, setRemoving] = useState<TwinLink | null>(null)
  const [notice, setNotice] = useState<TwinLink | null>(null)
  const online = node.conn === 'ready'
  const load = useCallback(async (): Promise<void> => {
    try {
      setLinks(await listTwinLinks(node.id))
      setFailed(false)
    } catch (e) {
      if (isUnsupported(e)) setUnsupported(true)
      else setFailed(true)
    }
  }, [node.id])
  useEffect(() => {
    if (online) void load()
  }, [online, load])
  // 这个节点的 daemon 还不认识分身连接：不显示这一块
  if (unsupported) return null
  return (
    <section className="space-y-3" data-testid="twin-links">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="font-semibold">{t('twinLinks.title')}</h3>
        <Button size="sm" disabled={!online} onClick={() => setAdding(true)} data-testid="twin-link-add">
          {t('twinLinks.add')}
        </Button>
      </div>
      <p className="text-sm text-fg-subtle">{t('twinLinks.hint')}</p>
      {failed ? (
        <div role="alert" className="text-sm text-danger">
          {t('twinLinks.loadFailed')}{' '}
          <Button size="sm" disabled={!online} onClick={() => void load()}>
            {t('todos.retry')}
          </Button>
        </div>
      ) : null}
      {links && !links.length ? <p className="text-sm text-fg-subtle">{t('twinLinks.none')}</p> : null}
      <ul className="space-y-2">
        {(links ?? []).map((link) => (
          <LinkRow key={link.node_id} link={link} online={online} onRemove={() => setRemoving(link)} />
        ))}
      </ul>
      {notice ? <Revoke link={notice} /> : null}
      {adding ? (
        <AddDialog
          node={node}
          links={links ?? []}
          onClose={() => setAdding(false)}
          onAdded={() => {
            setAdding(false)
            setNotice(null)
            void load()
          }}
        />
      ) : null}
      <RemoveDialog
        node={node}
        link={removing}
        onClose={() => setRemoving(null)}
        onRemoved={(link) => {
          setRemoving(null)
          setNotice(link)
          void load()
        }}
      />
    </section>
  )
}

function LinkRow({ link, online, onRemove }: { link: TwinLink; online: boolean; onRemove: () => void }) {
  const { t } = useTranslation()
  const status = link.status
  const open = status?.projects ?? []
  return (
    <li className="space-y-1 rounded border border-border p-3" data-testid="twin-link">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="flex flex-wrap items-center gap-2 font-medium">
          <span>{link.name}</span>
          {link.group ? <Chip>{link.group}</Chip> : null}
          <span className="inline-flex items-center gap-1 text-sm font-normal text-fg-subtle" data-testid="twin-link-online">
            <Dot tone={link.online ? 'success' : 'neutral'} />
            {link.online ? t('twinLinks.online') : t('twinLinks.offline')}
          </span>
        </p>
        <Button size="sm" variant="outline-danger" disabled={!online} onClick={onRemove} data-testid="twin-link-remove">
          {t('twinLinks.disconnect')}
        </Button>
      </div>
      {status ? (
        <>
          <p className="text-sm" data-testid="twin-link-projects">
            {open.length ? t('twinLinks.projects', { projects: open.join(', ') }) : t('twinLinks.noProjects')}
          </p>
          <p className="text-sm text-fg-subtle" data-testid="twin-link-remaining">
            {t('twinLinks.remaining', { sessions: status.remaining_sessions, dispatches: status.remaining_dispatches })}
          </p>
          {status.paused ? <p className="text-sm">{t('twinLinks.paused')}</p> : null}
          {!status.accept_links ? <p className="text-sm">{t('twinLinks.acceptOff')}</p> : null}
        </>
      ) : (
        <p className="text-sm text-fg-subtle">{t('twinLinks.noStatus')}</p>
      )}
    </li>
  )
}

/** 断开之后的提醒：A 只是忘掉这一条，B 上的设备记录要在 B 上吊销。这台设备也和 B 配过对时给一个去 B 的设备列表的入口。 */
function Revoke({ link }: { link: TwinLink }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const paired = useNodes((s) => !!s.byId[link.node_id])
  return (
    <p role="status" className="rounded border border-border p-3 text-sm" data-testid="twin-link-revoke">
      {t('twinLinks.disconnected', { name: link.name })}{' '}
      {paired ? (
        <Link to="/n/$node/settings" params={{ node: shortNodeId(link.node_id, order) }} search={{ tab: 'devices' }} className="text-accent-text">
          {t('twinLinks.openDevices', { name: link.name })}
        </Link>
      ) : (
        t('twinLinks.revokeThere', { name: link.name })
      )}
    </p>
  )
}

function RemoveDialog({ node, link, onClose, onRemoved }: { node: NodeEntry; link: TwinLink | null; onClose: () => void; onRemoved: (link: TwinLink) => void }) {
  const { t } = useTranslation()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const order = useNodes((s) => s.order)
  const paired = useNodes((s) => (link ? !!s.byId[link.node_id] : false))
  useEffect(() => setError(''), [link])
  return (
    <Dialog open={!!link} onOpenChange={(o) => !o && onClose()} title={t('twinLinks.disconnectTitle', { name: link?.name ?? '' })} description={t('twinLinks.disconnectBody', { name: link?.name ?? '' })} closeLabel={t('common.close')} testId="twin-link-remove-dialog">
      <div className="space-y-3">
        <p className="text-sm" data-testid="twin-link-revoke-hint">
          {t('twinLinks.revokeHint', { name: link?.name ?? '' })}
        </p>
        {link && paired ? (
          <Link to="/n/$node/settings" params={{ node: shortNodeId(link.node_id, order) }} search={{ tab: 'devices' }} className="text-sm text-accent-text">
            {t('twinLinks.openDevices', { name: link.name })}
          </Link>
        ) : null}
        {error ? (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        ) : null}
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={onClose}>
            {t('common.cancel')}
          </Button>
          <Button
            variant="danger"
            loading={busy}
            data-testid="twin-link-remove-confirm"
            onClick={() => {
              if (!link) return
              setBusy(true)
              setError('')
              removeTwinLink(node.id, link.node_id)
                .then(() => onRemoved(link))
                .catch((e: unknown) => setError(describeError(e, t)))
                .finally(() => setBusy(false))
            }}
          >
            {t('twinLinks.disconnect')}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}

function AddDialog({ node, links, onClose, onAdded }: { node: NodeEntry; links: TwinLink[]; onClose: () => void; onAdded: () => void }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const [picked, setPicked] = useState('')
  const [confirmGroup, setConfirmGroup] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const candidates = order.filter((id) => id !== node.id && byId[id])
  const target = picked ? byId[picked] : undefined
  const differ = !!target && groupsDiffer(node.info?.group, target.info?.group)
  const run = (): void => {
    if (!target) return
    setBusy(true)
    setError('')
    addTwinLink(node.id, target.id)
      .then(onAdded)
      .catch((e: unknown) => setError(describeLinkError(e, t)))
      .finally(() => setBusy(false))
  }
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={t('twinLinks.addTitle')} description={t('twinLinks.addBody', { name: node.name })} closeLabel={t('common.close')} testId="twin-link-add-dialog">
      <div className="space-y-3">
        {confirmGroup && target ? (
          <div className="space-y-2" data-testid="twin-link-group-confirm">
            <p className="text-sm">{t('twinLinks.groupDiffers', { a: node.name, groupA: node.info?.group || t('twinLinks.noGroup'), b: target.name, groupB: target.info?.group || t('twinLinks.noGroup') })}</p>
            <p className="text-sm text-fg-subtle">{t('twinLinks.groupWhy')}</p>
          </div>
        ) : (
          <fieldset className="space-y-2" disabled={busy}>
            <legend className="text-sm">{t('twinLinks.pick')}</legend>
            {!candidates.length ? <p className="text-sm text-fg-subtle">{t('twinLinks.noCandidates')}</p> : null}
            {candidates.map((id) => {
              const entry = byId[id]!
              const block = candidateBlock(entry.info, entry.conn === 'ready', links.some((l) => l.node_id === id))
              return (
                <label key={id} className="flex min-h-11 items-start gap-2" data-testid="twin-link-candidate" data-blocked={block ?? ''}>
                  <input type="radio" name="twin-link-node" className="mt-1" disabled={!!block} checked={picked === id} onChange={() => setPicked(id)} />
                  <span>
                    {entry.name}
                    {entry.info?.group ? <span className="ml-2 text-xs text-fg-subtle">{entry.info.group}</span> : null}
                    {block ? <span className="block text-xs text-fg-subtle">{t(`twinLinks.blocked.${block}`)}</span> : null}
                  </span>
                </label>
              )
            })}
          </fieldset>
        )}
        {busy ? <p className="text-sm text-fg-subtle" role="status">{t('twinLinks.connecting')}</p> : null}
        {error ? (
          <p role="alert" className="text-sm text-danger" data-testid="twin-link-error">
            {error}
          </p>
        ) : null}
        <div className="flex justify-end gap-2">
          <Button
            variant="outline"
            onClick={() => (confirmGroup && !busy ? setConfirmGroup(false) : onClose())}
          >
            {confirmGroup ? t('common.back') : t('common.cancel')}
          </Button>
          <Button
            variant="primary"
            loading={busy}
            disabled={!target || node.conn !== 'ready'}
            data-testid="twin-link-connect"
            onClick={() => {
              if (differ && !confirmGroup) setConfirmGroup(true)
              else run()
            }}
          >
            {differ && !confirmGroup ? t('twinLinks.next') : confirmGroup ? t('twinLinks.connectAnyway') : t('twinLinks.connect')}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
