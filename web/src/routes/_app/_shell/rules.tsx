import { createFileRoute } from '@tanstack/react-router'
import { RulesPage } from '@/pages/RulesPage'

export const Route = createFileRoute('/_app/_shell/rules')({ component: RulesPage })
