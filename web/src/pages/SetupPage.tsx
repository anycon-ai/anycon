import { useCallback, useMemo, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { Check, Download, Link2, PartyPopper } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { CenterLayout } from '@/components/CenterLayout'
import { PairForm } from '@/components/PairForm'
import { NotificationsStep } from '@/features/setup/NotificationsStep'
import { PasswordStep } from '@/features/setup/PasswordStep'
import { useLinkCode } from '@/features/pairing/useLinkCode'
import { detectOS } from '@/lib/device'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Card, CopyButton, Segmented } from '@/ui/Misc'

type Platform = 'macos' | 'linux' | 'windows' | 'other'

function guessPlatform(): Platform {
  const os = detectOS()
  if (os === 'macOS') return 'macos'
  if (os === 'Linux') return 'linux'
  if (os === 'Windows') return 'windows'
  return 'macos'
}

/** 首次向导：装 daemon → 配对 → 完成（每步可跳过）。 */
export function SetupPage({ code: legacyCode }: { code?: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const linkCode = useLinkCode('/setup', legacyCode)
  const [step, setStep] = useState<1 | 2 | 3>(linkCode ? 2 : 1)
  const [platform, setPlatform] = useState<Platform>(guessPlatform)
  const [pairedId, setPairedId] = useState<string | null>(null)
  // 刚配好一台节点：先问要不要设口令（可跳过），再进「完成」
  const [passwordAsked, setPasswordAsked] = useState(false)
  // 然后问要不要给这个浏览器开通知（可跳过；浏览器收不了推送时自己跳过）
  const [notifyAsked, setNotifyAsked] = useState(false)
  const notifyDone = useCallback(() => setNotifyAsked(true), [])
  const hub = useMemo(() => location.origin, [])
  const nodeCount = useNodes((s) => s.order.length)

  // 安装方式与 README / docs/deploy.md 一致：macOS 走 Homebrew tap，Linux 走发布页的安装脚本，Windows 走 PowerShell 安装脚本，
  // 其余平台从源码构建
  const init = [`anycon init --hub ${hub} --workspace ~/anycon`, 'anycon daemon']
  const install: Record<Platform, string[]> = {
    macos: ['brew install anycon-ai/tap/anycon', ...init],
    linux: ['curl -fsSL https://github.com/anycon-ai/anycon/releases/latest/download/install.sh | sh', ...init],
    windows: ['irm https://github.com/anycon-ai/anycon/releases/latest/download/install.ps1 | iex', ...init],
    other: [`# ${t('setup.installSourceHint')}`, 'cargo install --git https://github.com/anycon-ai/anycon anycon-cli --locked', ...init],
  }

  return (
    <CenterLayout wide>
      <ol className="mb-5 flex items-center justify-center gap-2 text-xs" aria-label={t('setup.steps')}>
        {[1, 2, 3].map((n) => (
          <li key={n} className={cn('flex items-center gap-2', n === step ? 'font-semibold text-fg' : 'text-fg-subtle')}>
            <span className={cn('grid size-5 place-items-center rounded-full border text-[0.6875rem]', n < step ? 'border-success bg-success text-white' : n === step ? 'border-accent text-accent-text' : 'border-border-strong')}>
              {n < step ? <Check className="size-3" /> : n}
            </span>
            {t(`setup.step${n}`)}
            {n < 3 ? <span className="h-px w-6 bg-border-strong" /> : null}
          </li>
        ))}
      </ol>

      <Card className="p-5" testId="setup-card">
        {step === 1 ? (
          <div className="space-y-4">
            <div className="flex items-center gap-2">
              <Download className="size-5 text-accent-text" />
              <h1 className="text-lg font-semibold">{t('setup.installTitle')}</h1>
            </div>
            <p className="text-sm text-fg-muted">{t('setup.installBody')}</p>
            <Segmented
              ariaLabel={t('setup.platform')}
              value={platform}
              onChange={setPlatform}
              options={[
                { value: 'macos', label: 'macOS' },
                { value: 'linux', label: 'Linux' },
                { value: 'windows', label: 'Windows' },
                { value: 'other', label: t('setup.otherPlatform') },
              ]}
            />
            <div className="rounded-md bg-bg-muted p-3">
              <pre className="overflow-x-auto font-mono text-[0.8125rem] leading-relaxed" data-testid="setup-commands">
                {install[platform].join('\n')}
              </pre>
              <div className="mt-2 flex justify-end">
                <CopyButton text={install[platform].filter((l) => !l.startsWith('#')).join('\n')} label={t('setup.copyCommands')} />
              </div>
            </div>
            <div className="flex justify-between gap-2">
              <Button variant="ghost" onClick={() => setStep(2)}>
                {t('setup.skip')}
              </Button>
              <Button variant="primary" onClick={() => setStep(2)} data-testid="setup-next">
                {t('setup.installed')}
              </Button>
            </div>
          </div>
        ) : null}

        {step === 2 ? (
          <div className="space-y-4">
            <div className="flex items-center gap-2">
              <Link2 className="size-5 text-accent-text" />
              <h1 className="text-lg font-semibold">{t('setup.pairTitle')}</h1>
            </div>
            <p className="text-sm text-fg-muted">
              {t('setup.pairBody')} <code className="rounded bg-bg-muted px-1.5 py-0.5 font-mono text-[0.8125rem]">anycon pair</code>
            </p>
            <div className="mx-auto max-w-xs pt-1">
              <PairForm
                initialCode={linkCode}
                fromLink={!!linkCode}
                onPaired={(id) => {
                  setPairedId(id)
                  setStep(3)
                }}
              />
            </div>
            <div className="flex justify-between">
              <Button variant="ghost" onClick={() => setStep(1)}>
                {t('common.back')}
              </Button>
              {nodeCount > 0 ? (
                <Button variant="ghost" onClick={() => setStep(3)}>
                  {t('setup.skip')}
                </Button>
              ) : null}
            </div>
          </div>
        ) : null}

        {step === 3 && pairedId && !passwordAsked ? <PasswordStep nodeId={pairedId} onDone={() => setPasswordAsked(true)} /> : null}

        {step === 3 && pairedId && passwordAsked && !notifyAsked ? <NotificationsStep onDone={notifyDone} /> : null}

        {step === 3 && (!pairedId || (passwordAsked && notifyAsked)) ? (
          <div className="space-y-4 text-center">
            <PartyPopper className="mx-auto size-10 text-accent-text" />
            <h1 className="text-lg font-semibold">{t('setup.doneTitle')}</h1>
            <p className="text-sm text-fg-muted">{t('setup.doneBody')}</p>
            <Button
              variant="primary"
              size="lg"
              data-testid="setup-finish"
              onClick={() => {
                const order = useNodes.getState().order
                const id = pairedId ?? order[0]
                if (id) void navigate({ to: '/n/$node', params: { node: shortNodeId(id, order) } })
                else void navigate({ to: '/' })
              }}
            >
              {t('setup.open')}
            </Button>
          </div>
        ) : null}
      </Card>
    </CenterLayout>
  )
}
