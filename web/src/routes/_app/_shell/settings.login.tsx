import { createFileRoute } from '@tanstack/react-router'
import { LoginMethodsPage } from '@/pages/settings/LoginMethodsPage'

export const Route = createFileRoute('/_app/_shell/settings/login')({ component: LoginMethodsPage })
