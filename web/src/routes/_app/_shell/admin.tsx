import { createFileRoute } from '@tanstack/react-router'
import { AdminPage } from '@/pages/AdminPage'

export const Route = createFileRoute('/_app/_shell/admin')({ component: AdminPage })
