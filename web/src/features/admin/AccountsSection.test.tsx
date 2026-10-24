// /admin 的账号管理（spec/web.md § 4.10）：搜索、加载更多（X-Next-Cursor）、授予 / 取消管理员、删除，以及错误码映射。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ApiAdminAccount } from '@/api/types'
import { useUi } from '@/stores/ui'
import { errorBody, installFakeApi, type ApiCall, type ApiHandler } from '@/test/fakeApi'
import { AccountsSection } from './AccountsSection'

const acct = (id: string, name: string, over: Partial<ApiAdminAccount> = {}): ApiAdminAccount => ({
  id,
  display_name: name,
  email: `${name.toLowerCase()}@example.com`,
  is_admin: false,
  admin_by_config: false,
  created_at_ms: 1_700_000_000_000,
  ...over,
})

/** 假 hub 每页给几条（hub 自己决定页大小；`limit` 只是上限）。 */
let PAGE = 10
let accounts: ApiAdminAccount[]

/** 假的 hub：q 是名字 / 邮箱子串或精确 id；每页 PAGE 条，游标就是下一条的下标。 */
function listAccounts(c: ApiCall) {
  const url = new URL(c.path, 'http://h')
  const q = (url.searchParams.get('q') ?? '').toLowerCase()
  const from = Number(url.searchParams.get('cursor') ?? 0)
  const all = accounts.filter((a) => !q || a.display_name.toLowerCase().includes(q) || (a.email ?? '').toLowerCase().includes(q) || a.id === url.searchParams.get('q'))
  const page = all.slice(from, from + PAGE)
  return { body: page, headers: from + PAGE < all.length ? { 'X-Next-Cursor': String(from + PAGE) } : {} }
}

function mount(extra: Record<string, ApiHandler> = {}, meId = 'me') {
  const api = installFakeApi({
    'GET /api/admin/accounts': listAccounts,
    'POST /api/admin/accounts/*': (c) => {
      const id = c.path.split('/')[4]!
      const a = accounts.find((x) => x.id === id)!
      a.is_admin = true
      return { body: a }
    },
    'DELETE /api/admin/accounts/*': (c) => {
      const parts = c.path.split('/')
      const id = parts[4]!
      if (parts[5] === 'admin') {
        const a = accounts.find((x) => x.id === id)!
        a.is_admin = false
        return { body: a }
      }
      accounts = accounts.filter((x) => x.id !== id)
      return undefined
    },
    ...extra,
  })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <AccountsSection meId={meId} />
    </QueryClientProvider>,
  )
  return api
}

const toasts = () => useUi.getState().toasts.map((t) => `${t.kind}:${t.text}`)
const rows = () => within(screen.getByTestId('admin-account-list')).getAllByRole('listitem')

beforeEach(() => {
  PAGE = 10
  useUi.setState({ toasts: [] })
  accounts = [
    acct('me', 'Root', { is_admin: true, admin_by_config: true }),
    acct('a1', 'Alice'),
    acct('a2', 'Bob', { is_admin: true }),
    acct('a3', 'Alicia'),
    acct('a4', 'Dave', { email: null }),
  ]
})
afterEach(() => vi.unstubAllGlobals())

describe('list and badges', () => {
  it('shows name, email (or a placeholder), joined date and the badges', async () => {
    mount()
    await screen.findByTestId('admin-account-me')
    const me = screen.getByTestId('admin-account-me')
    expect(me).toHaveTextContent('Root')
    expect(me).toHaveTextContent('You')
    expect(me).toHaveTextContent('Administrator')
    expect(screen.getByTestId('admin-by-config-me')).toHaveTextContent('Admin via ANYCON_ADMIN_EMAILS')
    const bob = screen.getByTestId('admin-account-a2')
    expect(bob).toHaveTextContent('Administrator')
    expect(within(bob).queryByText('Admin via ANYCON_ADMIN_EMAILS')).toBeNull()
    expect(screen.getByTestId('admin-account-a1')).not.toHaveTextContent('Administrator')
  })

  it('asks for a page size and shows what the first page holds', async () => {
    PAGE = 2
    const api = mount()
    await screen.findByTestId('admin-account-me')
    expect(rows()).toHaveLength(2)
    expect(api.callsTo('GET /api/admin/accounts')[0]!.path).toBe('/api/admin/accounts?limit=50')
  })

  it('an empty hub says so', async () => {
    accounts = []
    mount()
    expect(await screen.findByText('No accounts.')).toBeInTheDocument()
  })
})

describe('load more (X-Next-Cursor)', () => {
  it('appends the next page with the cursor from the header, and the button goes away on the last page', async () => {
    PAGE = 2
    const user = userEvent.setup()
    const api = mount()
    await screen.findByTestId('admin-account-me')
    await user.click(screen.getByTestId('admin-accounts-more'))
    await waitFor(() => expect(rows()).toHaveLength(4))
    expect(api.callsTo('GET /api/admin/accounts')[1]!.path).toContain('cursor=2')
    await user.click(screen.getByTestId('admin-accounts-more'))
    await waitFor(() => expect(rows()).toHaveLength(5))
    expect(screen.queryByTestId('admin-accounts-more')).toBeNull()
    expect(screen.getByTestId('admin-account-a4')).toHaveTextContent('no email')
  })

  it('no button at all when everything fits on one page', async () => {
    mount()
    await screen.findByTestId('admin-account-me')
    expect(screen.queryByTestId('admin-accounts-more')).toBeNull()
  })
})

describe('search', () => {
  it('sends q once typing pauses, replaces the list, and starts again from the first page', async () => {
    const user = userEvent.setup()
    const api = mount()
    await screen.findByTestId('admin-account-me')
    await user.type(screen.getByTestId('admin-account-search'), 'ali')
    await waitFor(() => expect(screen.getByTestId('admin-account-a1')).toBeInTheDocument())
    await waitFor(() => expect(rows()).toHaveLength(2)) // Alice, Alicia
    expect(screen.queryByTestId('admin-account-me')).toBeNull()
    const searches = api.callsTo('GET /api/admin/accounts').filter((c) => c.path.includes('q='))
    expect(searches).toHaveLength(1) // 一个字一个字敲不会每次都发请求
    expect(searches[0]!.path).toBe('/api/admin/accounts?limit=50&q=ali')
  })

  it('trims the term, and says when nothing matches', async () => {
    const user = userEvent.setup()
    const api = mount()
    await screen.findByTestId('admin-account-me')
    await user.type(screen.getByTestId('admin-account-search'), '  zzz ')
    expect(await screen.findByText('No accounts match your search.')).toBeInTheDocument()
    expect(api.calls.at(-1)!.path).toBe('/api/admin/accounts?limit=50&q=zzz')
  })

  it('clearing the box brings the full list back', async () => {
    const user = userEvent.setup()
    mount()
    await screen.findByTestId('admin-account-me')
    const box = screen.getByTestId('admin-account-search')
    await user.type(box, 'bob')
    await waitFor(() => expect(rows()).toHaveLength(1))
    await user.clear(box)
    await waitFor(() => expect(rows()).toHaveLength(5))
    expect(screen.getByTestId('admin-account-me')).toBeInTheDocument()
  })
})

describe('grant / revoke admin', () => {
  it('grant: POST …/admin, then the row shows as administrator', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('admin-grant-a1'))
    await waitFor(() => expect(screen.getByTestId('admin-account-a1')).toHaveTextContent('Administrator'))
    expect(api.callsTo('POST /api/admin/accounts/a1/admin')).toHaveLength(1)
    expect(api.calls.find((c) => c.method === 'POST')!.headers['X-Anycon']).toBe('1')
    expect(toasts()).toEqual(['success:Alice is now an administrator.'])
    expect(screen.getByTestId('admin-revoke-a1')).toBeInTheDocument()
  })

  it('revoke: DELETE …/admin, then the row is a normal account again', async () => {
    const user = userEvent.setup()
    const api = mount()
    await screen.findByTestId('admin-account-a2')
    await user.click(await screen.findByTestId('admin-revoke-a2'))
    await waitFor(() => expect(screen.getByTestId('admin-grant-a2')).toBeInTheDocument())
    expect(api.callsTo('DELETE /api/admin/accounts/a2/admin')).toHaveLength(1)
    expect(toasts()).toEqual(['success:Bob is no longer an administrator.'])
  })

  it('you cannot change or delete your own account (the hub would say cannot_modify_self)', async () => {
    mount()
    await screen.findByTestId('admin-account-me')
    expect(screen.getByTestId('admin-revoke-me')).toBeDisabled()
    expect(screen.getByTestId('admin-delete-me')).toBeDisabled()
    expect(screen.getByTestId('admin-revoke-me')).toHaveAttribute('title', expect.stringMatching(/your own account|ANYCON_ADMIN_EMAILS/))
  })

  it('an administrator set by ANYCON_ADMIN_EMAILS cannot be revoked here, and the button says why', async () => {
    accounts[1] = acct('a1', 'Alice', { is_admin: true, admin_by_config: true })
    mount({}, 'someone-else')
    const revoke = await screen.findByTestId('admin-revoke-a1')
    expect(revoke).toBeDisabled()
    expect(revoke).toHaveAttribute('title', expect.stringContaining('ANYCON_ADMIN_EMAILS'))
  })

  it.each([
    ['admin_by_config', 409, /set in the hub’s configuration \(ANYCON_ADMIN_EMAILS\)/],
    ['bootstrap_admin', 409, /first administrator account/],
    ['cannot_modify_self', 400, /your own account/],
    ['not_found', 404, /Not found/],
  ] as const)('a refused revoke (%s) is shown as an error and the row is unchanged', async (code, status, text) => {
    const user = userEvent.setup()
    mount({ 'DELETE /api/admin/accounts/*': () => ({ status, body: errorBody(code) }) })
    await user.click(await screen.findByTestId('admin-revoke-a2'))
    await waitFor(() => expect(toasts()).toHaveLength(1))
    expect(toasts()[0]).toMatch(/^error:/)
    expect(toasts()[0]).toMatch(text)
    expect(screen.getByTestId('admin-account-a2')).toHaveTextContent('Administrator')
  })

  it('a refused grant (404: the account was deleted meanwhile) is shown as an error', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/admin/accounts/*': () => ({ status: 404, body: errorBody('not_found') }) })
    await user.click(await screen.findByTestId('admin-grant-a1'))
    await waitFor(() => expect(toasts()).toEqual(['error:Not found.']))
  })
})

describe('delete', () => {
  it('asks first, naming the account and what happens; confirming deletes it and the row disappears', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('admin-delete-a1'))
    const dialog = await screen.findByTestId('admin-delete-dialog')
    expect(dialog).toHaveTextContent('Delete Alice?')
    expect(dialog).toHaveTextContent(/Session history lives on that person’s nodes and is not deleted/)
    expect(api.callsTo('DELETE /api/admin/accounts/a1')).toHaveLength(0)
    await user.click(screen.getByTestId('admin-delete-confirm'))
    await waitFor(() => expect(screen.queryByTestId('admin-account-a1')).toBeNull())
    expect(api.callsTo('DELETE /api/admin/accounts/a1')).toHaveLength(1)
    expect(screen.queryByTestId('admin-delete-dialog')).toBeNull()
    expect(toasts()).toEqual(['success:Alice was deleted.'])
  })

  it('cancel deletes nothing', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('admin-delete-a1'))
    await user.click(await screen.findByRole('button', { name: 'Cancel' }))
    await waitFor(() => expect(screen.queryByTestId('admin-delete-dialog')).toBeNull())
    expect(api.calls.some((c) => c.method === 'DELETE')).toBe(false)
  })

  it.each([
    ['bootstrap_admin', 409, /first administrator account/],
    ['cannot_modify_self', 400, /your own account/],
    ['not_found', 404, /Not found/],
  ] as const)('%s is explained inside the dialog, which stays open', async (code, status, text) => {
    const user = userEvent.setup()
    mount({ 'DELETE /api/admin/accounts/*': () => ({ status, body: errorBody(code) }) })
    await user.click(await screen.findByTestId('admin-delete-a1'))
    await user.click(await screen.findByTestId('admin-delete-confirm'))
    expect(await screen.findByTestId('admin-delete-error')).toHaveTextContent(text)
    expect(screen.getByTestId('admin-delete-dialog')).toBeInTheDocument()
    expect(screen.getByTestId('admin-account-a1')).toBeInTheDocument()
  })

  it('the error is cleared when the dialog is opened for another account', async () => {
    const user = userEvent.setup()
    mount({ 'DELETE /api/admin/accounts/*': () => ({ status: 409, body: errorBody('bootstrap_admin') }) })
    await user.click(await screen.findByTestId('admin-delete-a1'))
    await user.click(await screen.findByTestId('admin-delete-confirm'))
    await screen.findByTestId('admin-delete-error')
    await user.click(screen.getByRole('button', { name: 'Cancel' }))
    await waitFor(() => expect(screen.queryByTestId('admin-delete-dialog')).toBeNull())
    await user.click(screen.getByTestId('admin-delete-a2'))
    expect(await screen.findByTestId('admin-delete-dialog')).toHaveTextContent('Delete Bob?')
    expect(screen.queryByTestId('admin-delete-error')).toBeNull()
  })
})
