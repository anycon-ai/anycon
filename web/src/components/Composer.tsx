import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ClipboardEvent, type DragEvent, type KeyboardEvent, type ReactNode } from 'react'
import { ArrowUp, Paperclip, Plus, SlidersHorizontal, Square } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { AttachmentRef } from '@/protocol'
import { addFiles, clearItems, hydrateFromDraft, removeItem, retryItem, type AttachContext, type AttachNotice } from '@/features/attachments/composerUploads'
import { describeAttachNotice } from '@/features/attachments/errors'
import { ComposerAttachments } from '@/features/attachments/ui/ComposerAttachments'
import { useIsTouch, useIsWide } from '@/lib/useMediaQuery'
import { useDrafts, type AttachmentItem } from '@/stores/drafts'
import { usePrefs } from '@/stores/prefs'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Menu, MenuContent, MenuItem, MenuTrigger } from '@/ui/Menu'
import { Sheet } from '@/ui/Overlay'

/** 输入区的图片附件（ADR-35）。不传这个 prop，「＋」菜单里就没有「添加文件或照片」，粘贴 / 拖放也不处理。 */
export interface ComposerAttachmentConfig {
  nodeId: string
  /** agent 收的 mime（已经和 daemon 的上传白名单取过交集）。 */
  accept: readonly string[]
  /** 有值时「添加文件或照片」禁用并说明原因（节点不在线）。 */
  disabledReason?: string
  /** 测试注入（规整用的编解码、上传的重试间隔）。 */
  test?: Pick<AttachContext, 'codec' | 'upload'>
}

export interface ComposerProps {
  /** drafts store 的 key：会话 id，或新建会话用的 `new:<node>:<project>`。 */
  draftKey: string
  /** `attachments` 是上传完的图片（`blob.put_end` 的结果），没有图片是空数组。只有图片、没有文字也会发。 */
  onSend: (text: string, attachments: AttachmentRef[]) => void | Promise<void>
  attachments?: ComposerAttachmentConfig
  onStop?: () => void
  running?: boolean
  /** 无法发送的原因（离线创建、没有可用 agent 等）。有值时输入框仍可用，发送按钮禁用。 */
  blockedReason?: string
  /** 宽屏贴在输入框顶上的上下文栏（新建会话的项目 / 工作位置）。窄屏不显示，调用方把它们放进 `options('stacked')`。 */
  context?: ReactNode
  /** 宽屏输入框上方靠右的一小块（会话页的分支 chip）。窄屏不显示，调用方把它放进 `options('stacked')`。 */
  aside?: ReactNode
  /** 选项栏（SettingsControls）。宽屏是输入框下面的一行，窄屏收进「⚙」底部弹层（堆叠布局）。 */
  options?: (layout: 'inline' | 'stacked') => ReactNode
  /** 输入框上方的额外提示（新建会话的「危险模式没被选中」等）。 */
  notice?: ReactNode
  /** 运行中输入内容会排队，显示「将在本轮结束后发送」。 */
  queueNotice?: boolean
  placeholder?: string
  autoFocus?: boolean
  /** 改变这个值会把输入框聚焦（比如从「编辑未发送消息」回填后）。 */
  focusToken?: number
  sending?: boolean
}

const MAX_ROWS = 10
const NO_ITEMS: AttachmentItem[] = []

/**
 * 输入区：圆角框里是缩略图、「＋」菜单、输入框、停止 / 发送；框上面可以贴一条上下文栏、右侧可以放一小块，框下面一行是选项。
 * 自动增高（最多 10 行）、Enter 规则、草稿持久化、运行中排队、停止。
 */
export function Composer({ draftKey, onSend, attachments: attach, onStop, running, blockedReason, context, aside, options, notice, queueNotice, placeholder, autoFocus, focusToken, sending }: ComposerProps) {
  const { t } = useTranslation()
  const ref = useRef<HTMLTextAreaElement>(null)
  const text = useDrafts((s) => s.byKey[draftKey]?.text ?? '')
  const setText = useDrafts((s) => s.setText)
  const load = useDrafts((s) => s.load)
  const enterBehavior = usePrefs((s) => s.enterBehavior)
  const wide = useIsWide()
  const touch = useIsTouch()
  const [sheet, setSheet] = useState(false)
  const composing = useRef(false)
  const stopRef = useRef<HTMLButtonElement>(null)
  const [stopArmed, setStopArmed] = useState(false)
  const items = useDrafts((s) => s.items[draftKey])
  const draftAttachments = useDrafts((s) => s.byKey[draftKey]?.attachments)
  const fileRef = useRef<HTMLInputElement>(null)
  const [dragging, setDragging] = useState(false)
  const dragDepth = useRef(0)

  useEffect(() => {
    void load(draftKey)
  }, [draftKey, load])

  const notify = useCallback((n: AttachNotice) => toast.info(describeAttachNotice(n, t)), [t])

  // 刷新后：草稿里存着传完的附件，还没有列表时用它们恢复。节点在线时顺便确认这些 blob 还在（放太久会被 daemon 回收，
  // 回收了的改走「从缓存重传」）；离线 / 还没拿到节点信息时只恢复缩略图，不探
  useEffect(() => {
    if (items !== undefined || !draftAttachments?.length || !attach) return
    hydrateFromDraft(draftKey, draftAttachments, attach.disabledReason ? undefined : { draftKey, nodeId: attach.nodeId, accepted: attach.accept, notify, ...attach.test })
  }, [items, draftAttachments, draftKey, attach, notify])

  useEffect(() => {
    if (autoFocus || focusToken) ref.current?.focus()
  }, [autoFocus, focusToken])

  // 自动增高
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    const line = parseFloat(getComputedStyle(el).lineHeight) || 22
    const max = line * MAX_ROWS + 16
    el.style.height = `${Math.min(el.scrollHeight, max)}px`
    el.style.overflowY = el.scrollHeight > max ? 'auto' : 'hidden'
  }, [text])

  const enterSends = enterBehavior === 'send' || (enterBehavior === 'auto' && !touch)
  const list = attach ? (items ?? NO_ITEMS) : NO_ITEMS
  const uploading = list.some((i) => i.state === 'uploading' || i.checking)
  const failed = list.some((i) => i.state === 'error')
  const readyRefs = list.flatMap((i) => (i.state === 'ready' && i.ref ? [i.ref] : []))
  // 上传中不能发（消息得带上传完的引用）；传失败的也先处理掉（重试或移除），免得悄悄少发一张
  const attachBlock = uploading ? t('composer.attachUploading') : failed ? t('composer.attachFailedBlock') : undefined
  const canSend = (text.trim().length > 0 || readyRefs.length > 0) && !blockedReason && !sending && !attachBlock

  const submit = (): void => {
    if (!canSend) return
    const value = text.trim()
    setText(draftKey, '')
    if (attach) clearItems(draftKey)
    void onSend(value, readyRefs)
  }

  // ---- 图片附件：选图 / 粘贴 / 拖放 ----
  const handleFiles = (files: File[]): void => {
    if (!attach || files.length === 0) return
    if (attach.disabledReason) return toast.info(attach.disabledReason)
    addFiles(files, { draftKey, nodeId: attach.nodeId, accepted: attach.accept, notify, ...attach.test })
  }
  const onPaste = (e: ClipboardEvent<HTMLTextAreaElement>): void => {
    if (!attach) return
    const data = e.clipboardData
    let files = Array.from(data?.files ?? [])
    if (files.length === 0) files = Array.from(data?.items ?? []).flatMap((i) => (i.kind === 'file' ? [i.getAsFile()].filter((f): f is File => f !== null) : []))
    if (files.length === 0) return
    // 同时带文字的（从 Word、网页复制）按文字粘贴
    if (data?.getData('text/plain')) return
    e.preventDefault()
    handleFiles(files)
  }
  const hasFiles = (e: DragEvent): boolean => Array.from(e.dataTransfer?.types ?? []).includes('Files')
  const onDragEnter = (e: DragEvent): void => {
    if (!attach || !hasFiles(e)) return
    e.preventDefault()
    dragDepth.current += 1
    setDragging(true)
  }
  const onDragOver = (e: DragEvent): void => {
    if (!attach || !hasFiles(e)) return
    e.preventDefault() // 不阻止的话浏览器不会触发 drop
    if (e.dataTransfer) e.dataTransfer.dropEffect = attach.disabledReason ? 'none' : 'copy'
  }
  const onDragLeave = (e: DragEvent): void => {
    if (!attach || !hasFiles(e)) return
    dragDepth.current = Math.max(0, dragDepth.current - 1)
    if (dragDepth.current === 0) setDragging(false)
  }
  const onDrop = (e: DragEvent): void => {
    if (!attach || !hasFiles(e)) return
    e.preventDefault()
    dragDepth.current = 0
    setDragging(false)
    handleFiles(Array.from(e.dataTransfer?.files ?? []))
  }

  // Esc：运行中先聚焦「停止」，再按一次确认停止（spec/web.md § 9）
  useEffect(() => {
    if (!stopArmed) return
    const id = setTimeout(() => setStopArmed(false), 4000)
    return () => clearTimeout(id)
  }, [stopArmed])
  useEffect(() => {
    if (!running) setStopArmed(false)
  }, [running])
  const onEscape = (): void => {
    if (!running || !onStop) return
    if (stopArmed) {
      setStopArmed(false)
      onStop()
    } else {
      setStopArmed(true)
      stopRef.current?.focus()
    }
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>): void => {
    // 按住不放（`e.repeat`）只算第一下：长按 Esc 不能「先装填再确认停止」，长按 Enter 不能连发
    if (e.key === 'Escape') return e.repeat ? undefined : onEscape()
    if (e.key !== 'Enter' || composing.current || e.nativeEvent.isComposing) return
    const mod = e.metaKey || e.ctrlKey
    if (enterSends ? !e.shiftKey : mod) {
      e.preventDefault()
      if (!e.repeat) submit()
    }
  }

  return (
    <div className="shrink-0 bg-bg" data-testid="composer">
      <div
        className="relative mx-auto w-full max-w-3xl px-3 pb-[calc(1.5rem+env(safe-area-inset-bottom))] pt-2 md:px-4"
        data-testid="composer-dropzone"
        data-dragging={dragging}
        onDragEnter={onDragEnter}
        onDragOver={onDragOver}
        onDragLeave={onDragLeave}
        onDrop={onDrop}
      >
        {dragging ? (
          <div className="pointer-events-none absolute inset-1 z-10 flex items-center justify-center rounded-2xl border-2 border-dashed border-accent bg-accent-subtle/90 text-sm font-medium text-accent-text" data-testid="composer-drop-hint">
            {attach?.disabledReason ?? t('composer.dropHint')}
          </div>
        ) : null}
        {notice}
        {queueNotice && running && (text.trim() || readyRefs.length > 0) ? (
          <p className="mb-1 px-1 text-xs text-info" data-testid="composer-queue-notice">
            {t('composer.queueNotice')}
          </p>
        ) : null}
        {blockedReason ? (
          <p className="mb-1 px-1 text-xs text-warning" data-testid="composer-blocked">
            {blockedReason}
          </p>
        ) : null}
        {wide && aside ? (
          <div className="mb-1 flex min-w-0 justify-end px-1" data-testid="composer-aside">
            {aside}
          </div>
        ) : null}
        {wide && context ? (
          // 托盘比输入框窄一点、垫在它后面：输入框的圆角压住托盘的下沿
          <div className="mx-4 -mb-3 rounded-t-2xl border border-b-0 border-border bg-bg-subtle px-2 pb-3.5 pt-1" data-testid="composer-context">
            {context}
          </div>
        ) : null}
        <div className="relative rounded-2xl border border-border-strong bg-bg shadow-sm transition-colors focus-within:border-accent focus-within:ring-2 focus-within:ring-accent/20" data-testid="composer-box">
          {attach && list.length > 0 ? <ComposerAttachments nodeId={attach.nodeId} items={list} onRemove={(id) => removeItem(draftKey, attach.nodeId, id)} onRetry={(id) => retryItem(draftKey, id)} /> : null}
          <div className="flex items-end gap-1 p-2">
            {!wide && options ? (
              <Button variant="ghost" size="icon" aria-label={t('composer.options')} onClick={() => setSheet(true)} data-testid="composer-options-button" className="shrink-0 rounded-full">
                <SlidersHorizontal className="size-4" />
              </Button>
            ) : null}
            {attach ? (
              <>
                <Menu>
                  <MenuTrigger asChild>
                    <Button variant="ghost" size="icon" aria-label={t('composer.add')} title={t('composer.add')} data-testid="composer-plus" className="shrink-0 rounded-full">
                      <Plus className="size-5" />
                    </Button>
                  </MenuTrigger>
                  <MenuContent align="start" className="w-60">
                    <MenuItem icon={<Paperclip className="mt-0.5 size-4" />} disabled={!!attach.disabledReason} hint={attach.disabledReason} onSelect={() => fileRef.current?.click()} testId="composer-attach">
                      {t('composer.addFiles')}
                    </MenuItem>
                  </MenuContent>
                </Menu>
                <input
                  ref={fileRef}
                  type="file"
                  multiple
                  accept={[...attach.accept, 'image/*'].join(',')}
                  className="hidden"
                  tabIndex={-1}
                  aria-hidden
                  disabled={!!attach.disabledReason}
                  data-testid="composer-file-input"
                  onChange={(e) => {
                    handleFiles(Array.from(e.target.files ?? []))
                    e.target.value = '' // 同一张图可以再选一次
                  }}
                />
              </>
            ) : null}
            <textarea
              ref={ref}
              data-testid="composer-input"
              rows={1}
              value={text}
              onChange={(e) => setText(draftKey, e.target.value)}
              onKeyDown={onKeyDown}
              onPaste={onPaste}
              onCompositionStart={() => (composing.current = true)}
              onCompositionEnd={() => (composing.current = false)}
              placeholder={placeholder ?? t('composer.placeholder')}
              aria-label={t('composer.label')}
              enterKeyHint={enterSends ? 'send' : 'enter'}
              className="max-h-[60dvh] min-h-9 flex-1 resize-none bg-transparent px-1.5 py-1.5 text-[0.9375rem] leading-6 placeholder:text-fg-subtle focus:outline-none"
            />
            {running && onStop ? (
              <Button ref={stopRef} variant="danger" size="icon" aria-label={t('composer.stop')} title={stopArmed ? t('composer.stopConfirm') : t('composer.stop')} onClick={onStop} onKeyDown={(e) => e.key === 'Escape' && !e.repeat && onEscape()} data-testid="composer-stop" data-armed={stopArmed} className={cn('shrink-0 rounded-full', stopArmed && 'ring-2 ring-danger ring-offset-2 ring-offset-bg')}>
                <Square className="size-4 fill-current" />
              </Button>
            ) : null}
            <Button variant="primary" size="icon" aria-label={running ? t('composer.queue') : t('composer.send')} title={blockedReason ?? attachBlock ?? (running ? t('composer.queue') : t('composer.send'))} onClick={submit} disabled={!canSend} loading={sending} data-testid="composer-send" className="shrink-0 rounded-full">
              <ArrowUp className="size-4" />
            </Button>
          </div>
        </div>
        {wide && options ? <div className="mt-1.5">{options('inline')}</div> : null}
        {!wide && options ? (
          <Sheet open={sheet} onOpenChange={setSheet} title={t('composer.options')} closeLabel={t('common.close')} testId="options-sheet">
            {options('stacked')}
          </Sheet>
        ) : null}
      </div>
    </div>
  )
}
