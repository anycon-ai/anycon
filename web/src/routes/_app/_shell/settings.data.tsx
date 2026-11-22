import { createFileRoute } from '@tanstack/react-router'
import { DataPage } from '@/pages/settings/DataPage'

export const Route = createFileRoute('/_app/_shell/settings/data')({ component: DataPage })
