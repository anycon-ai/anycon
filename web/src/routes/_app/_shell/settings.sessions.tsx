import { createFileRoute } from '@tanstack/react-router'
import { BrowserSessionsPage } from '@/pages/settings/BrowserSessionsPage'

export const Route = createFileRoute('/_app/_shell/settings/sessions')({ component: BrowserSessionsPage })
