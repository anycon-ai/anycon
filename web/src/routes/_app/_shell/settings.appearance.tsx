import { createFileRoute } from '@tanstack/react-router'
import { AppearancePage } from '@/pages/settings/AppearancePage'

export const Route = createFileRoute('/_app/_shell/settings/appearance')({ component: AppearancePage })
