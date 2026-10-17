import { createFileRoute } from '@tanstack/react-router'
import { ProfilePage } from '@/pages/settings/ProfilePage'

export const Route = createFileRoute('/_app/_shell/settings/')({ component: ProfilePage })
