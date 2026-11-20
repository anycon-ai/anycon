import { createFileRoute } from '@tanstack/react-router'
import { CapturePage } from '@/pages/CapturePage'

export const Route = createFileRoute('/_app/_shell/capture')({ component: CapturePage })
