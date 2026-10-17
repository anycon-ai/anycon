import { Link, Outlet } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { PageHeader } from './PageHeader'

export function SettingsLayout() {
  const { t } = useTranslation()
  const items = [
    { to: '/settings', label: t('settings.profile'), exact: true, id: 'profile' },
    { to: '/settings/login', label: t('settings.login'), id: 'login' },
    { to: '/settings/sessions', label: t('settings.sessions'), id: 'sessions' },
    { to: '/settings/devices', label: t('settings.devices'), id: 'devices' },
    { to: '/settings/notifications', label: t('settings.notifications'), id: 'notifications' },
    { to: '/settings/appearance', label: t('settings.appearance'), id: 'appearance' },
  ] as const
  return (
    <>
      <PageHeader crumbs={[{ label: t('settings.title') }]} showNodeStatus={false} />
      <div className="scroll-thin min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-4xl flex-col gap-6 p-4 md:flex-row md:p-6">
          <nav aria-label={t('settings.title')} className="flex shrink-0 gap-1 overflow-x-auto md:w-48 md:flex-col">
            {items.map((i) => (
              <Link
                key={i.to}
                to={i.to}
                activeOptions={{ exact: 'exact' in i && i.exact }}
                data-testid={`settings-nav-${i.id}`}
                className="rounded-md px-3 py-2 text-sm whitespace-nowrap text-fg-muted hover:bg-bg-hover data-[status=active]:bg-accent-subtle data-[status=active]:font-medium data-[status=active]:text-fg"
              >
                {i.label}
              </Link>
            ))}
          </nav>
          <div className="min-w-0 flex-1">
            <Outlet />
          </div>
        </div>
      </div>
    </>
  )
}
