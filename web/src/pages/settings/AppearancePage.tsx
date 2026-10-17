import { useTranslation } from 'react-i18next'
import { usePrefs, type EnterBehavior, type FontSize, type LanguagePref, type ThemePref, type ThinkingDisplay } from '@/stores/prefs'
import { Card, Segmented, Switch } from '@/ui/Misc'

function Row({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-2 px-4 py-3 sm:flex-row sm:items-center sm:justify-between sm:gap-6">
      <div className="min-w-0">
        <p className="text-sm font-medium">{label}</p>
        {hint ? <p className="text-xs text-fg-subtle">{hint}</p> : null}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  )
}

export function AppearancePage() {
  const { t } = useTranslation()
  const p = usePrefs()
  return (
    <div className="space-y-4">
      <h1 className="text-xl font-semibold">{t('settings.appearance')}</h1>
      <Card className="divide-y divide-border" testId="appearance">
        <Row label={t('appearance.theme')}>
          <Segmented<ThemePref>
            ariaLabel={t('appearance.theme')}
            value={p.theme}
            onChange={(v) => p.set({ theme: v })}
            options={[
              { value: 'system', label: t('appearance.system'), testId: 'theme-system' },
              { value: 'light', label: t('appearance.light'), testId: 'theme-light' },
              { value: 'dark', label: t('appearance.dark'), testId: 'theme-dark' },
            ]}
          />
        </Row>
        <Row label={t('appearance.language')}>
          <Segmented<LanguagePref>
            ariaLabel={t('appearance.language')}
            value={p.language}
            onChange={(v) => p.set({ language: v })}
            options={[
              { value: 'auto', label: t('appearance.auto') },
              { value: 'en', label: 'English', testId: 'lang-en' },
              { value: 'zh', label: '中文', testId: 'lang-zh' },
            ]}
          />
        </Row>
        <Row label={t('appearance.fontSize')}>
          <Segmented<FontSize>
            ariaLabel={t('appearance.fontSize')}
            value={p.fontSize}
            onChange={(v) => p.set({ fontSize: v })}
            options={[
              { value: 'sm', label: t('appearance.small') },
              { value: 'md', label: t('appearance.medium') },
              { value: 'lg', label: t('appearance.large') },
            ]}
          />
        </Row>
        <Row label={t('appearance.compact')} hint={t('appearance.compactHint')}>
          <Switch label={t('appearance.compact')} checked={p.compact} onCheckedChange={(v) => p.set({ compact: v })} />
        </Row>
        <Row label={t('appearance.thinking')} hint={t('appearance.thinkingHint')}>
          <Segmented<ThinkingDisplay>
            ariaLabel={t('appearance.thinking')}
            value={p.thinkingDisplay}
            onChange={(v) => p.set({ thinkingDisplay: v })}
            options={[
              { value: 'collapsed', label: t('appearance.collapsed') },
              { value: 'expanded', label: t('appearance.expanded') },
              { value: 'hidden', label: t('appearance.hidden') },
            ]}
          />
        </Row>
        <Row label={t('appearance.enter')} hint={t('appearance.enterHint')}>
          <Segmented<EnterBehavior>
            ariaLabel={t('appearance.enter')}
            value={p.enterBehavior}
            onChange={(v) => p.set({ enterBehavior: v })}
            options={[
              { value: 'auto', label: t('appearance.auto') },
              { value: 'send', label: t('appearance.enterSend') },
              { value: 'newline', label: t('appearance.enterNewline') },
            ]}
          />
        </Row>
      </Card>
    </div>
  )
}
