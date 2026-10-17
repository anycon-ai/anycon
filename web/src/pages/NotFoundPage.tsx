import { Link, useRouter } from '@tanstack/react-router'
import { Compass } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'
import { EmptyState } from '@/ui/Misc'

export function NotFoundPage({ title, body }: { title?: string; body?: string }) {
  const { t } = useTranslation()
  const router = useRouter()
  return (
    <div className="flex min-h-[60dvh] items-center justify-center" data-testid="not-found">
      <EmptyState
        icon={<Compass className="size-10" />}
        title={title ?? t('notFound.title')}
        action={
          <div className="flex gap-2">
            <Button onClick={() => router.history.back()}>{t('common.back')}</Button>
            <Link to="/">
              <Button variant="primary">{t('notFound.home')}</Button>
            </Link>
          </div>
        }
      >
        {body ?? t('notFound.body')}
      </EmptyState>
    </div>
  )
}
