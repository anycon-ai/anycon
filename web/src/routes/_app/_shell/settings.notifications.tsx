import { createFileRoute } from '@tanstack/react-router'
import { NotificationsPage } from '@/pages/settings/NotificationsPage'

export const Route = createFileRoute('/_app/_shell/settings/notifications')({ component: NotificationsPage })
