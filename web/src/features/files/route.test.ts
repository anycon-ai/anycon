// 路由表里的 `/n/:node/files/*`：路径整个进 `_splat`（已解码），空路径也能匹配。
import { QueryClient } from '@tanstack/react-query'
import { describe, expect, it } from 'vitest'
import { createAppRouter } from '@/router'

function match(url: string): { id: string; params: Record<string, string> } | undefined {
  const router = createAppRouter(new QueryClient())
  const matches = router.matchRoutes(url, {})
  const last = matches[matches.length - 1]
  return last ? { id: last.routeId, params: last.params as Record<string, string> } : undefined
}

describe('/n/:node/files/*', () => {
  const ID = '/_app/_shell/n/$node/files/$'

  it('captures the whole path, decoded, as the splat', () => {
    expect(match('/n/abcd1234/files/app/src/main.ts')).toEqual({ id: ID, params: expect.objectContaining({ node: 'abcd1234', _splat: 'app/src/main.ts' }) })
    expect(match('/n/abcd1234/files/my%20proj/dir%20%231/a%3Fb%25c%20%E4%BD%A0%E5%A5%BD.txt')?.params._splat).toBe('my proj/dir #1/a?b%c 你好.txt')
    expect(match('/n/abcd1234/files/app/.anycon/worktrees/fix')?.params._splat).toBe('app/.anycon/worktrees/fix')
  })

  it('matches with an empty path (the project list), with or without the trailing slash', () => {
    for (const url of ['/n/abcd1234/files', '/n/abcd1234/files/']) expect(match(url)).toEqual({ id: ID, params: expect.objectContaining({ _splat: '' }) })
  })

  it('does not swallow the other node routes', () => {
    expect(match('/n/abcd1234/settings')?.id).toBe('/_app/_shell/n/$node/settings')
    expect(match('/n/abcd1234/p/app')?.id).toBe('/_app/_shell/n/$node/p/$project')
  })
})
