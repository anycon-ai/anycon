import { createFileRoute } from '@tanstack/react-router'
import { QuotaPage } from '@/pages/settings/QuotaPage'

export const Route = createFileRoute('/_app/_shell/settings/quota')({ component: QuotaPage })
