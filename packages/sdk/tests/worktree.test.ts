// ADR-34：每会话 worktree 与 git。覆盖 sessions.create 的 workspace / prompt、Session.worktree、
// gitStatus / diff / syncBase、archive / delete 的处置参数、projects.branches、worktrees.list / remove。
// 每个用例断言 SDK 发出的参数（不传的字段不发）和对返回值的解析；fake daemon 见 tests/fake-daemon.ts。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import { AnyconRpcError, type Crypto } from '@anycon/protocol'

import {
  RpcError,
  connect,
  type Node,
  type SessionChanged,
  type StreamEvent,
} from '../src/index.js'
import type { Env } from './harness.js'
import { FAST, cryptoImpls, startEnv, until } from './harness.js'

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

/** 起 fake hub + daemon、配对、连上，跑完 `fn` 再清理。 */
async function withNode(
  crypto: Crypto,
  fn: (node: Node, env: Env) => Promise<void>,
): Promise<void> {
  const env = await startEnv(crypto)
  try {
    await env.pair()
    const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
    try {
      await fn(node, env)
    } finally {
      await node.close()
    }
  } finally {
    await env.stop()
  }
}

/** daemon 收到的某个方法的全部 params（按到达顺序）。 */
const paramsOf = (env: Env, method: string): Array<Record<string, unknown>> =>
  env.daemon.requests
    .filter((r) => r.method === method)
    .map((r) => r.params as Record<string, unknown>)

/** 断言 promise 以 `RpcError` 失败，且稳定码 / reason 符合。 */
async function rejectsRpc(p: Promise<unknown>, code: string, reason?: string): Promise<RpcError> {
  try {
    await p
  } catch (err) {
    assert.ok(err instanceof RpcError, `expected RpcError, got ${String(err)}`)
    assert.equal(err.code, code)
    if (reason !== undefined) assert.equal(err.reason, reason)
    return err
  }
  assert.fail('expected the call to fail')
}

for (const impl of cryptoImpls) {
  describe(`worktree 与 git（${impl.label}）`, () => {
    it('sessions.create：不带 workspace / prompt 时线路上一个都不发（SDK 不替调用方选 worktree.default）', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const session = await node.sessions.create({ agent: 'mock', project: 'app' })
        assert.deepEqual(paramsOf(env, 'session.create'), [
          { agent: 'mock', project: 'app', settings: {} },
        ])
        assert.equal(session.worktree, undefined)
        // 空 prompt 当作没有：既不发 prompt 也不生成 message_id
        await node.sessions.create({ agent: 'mock', project: 'app', prompt: '' })
        assert.deepEqual(paramsOf(env, 'session.create')[1], {
          agent: 'mock',
          project: 'app',
          settings: {},
        })
        // 显式 local 原样发出
        await node.sessions.create({ agent: 'mock', project: 'app', workspace: { mode: 'local' } })
        assert.deepEqual(paramsOf(env, 'session.create')[2]?.workspace, { mode: 'local' })
      })
    })

    it('sessions.create：worktree 模式——参数原样发出，worktree 先 creating，随 session.changed 变 active，git 事件进事件流', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const changes: SessionChanged[] = []
        node.on('session_changed', (c) => changes.push(c))
        const session = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          settings: { model: 'mock-small' },
          workspace: { mode: 'worktree', base: 'origin/main', branch: 'feature/login' },
        })
        assert.deepEqual(paramsOf(env, 'session.create'), [
          {
            agent: 'mock',
            project: 'app',
            settings: { model: 'mock-small' },
            workspace: { mode: 'worktree', base: 'origin/main', branch: 'feature/login' },
          },
        ])
        // create() 返回时 worktree 还在创建
        assert.equal(session.worktree?.state, 'creating')
        assert.equal(session.worktree?.branch, 'feature/login')
        assert.equal(session.worktree?.base, 'origin/main')
        assert.match(session.worktree?.base_sha ?? '', /^[0-9a-f]{40}$/)
        assert.equal(session.worktree?.path, '.anycon/worktrees/wt-1')
        const worktreeId = session.worktree?.id
        assert.match(worktreeId ?? '', /^wt_/)

        // session.changed 带 worktree：事件 payload 里有，句柄上的 worktree 和 summary 随之更新
        await until(() => session.worktree?.state === 'active', 3000, 'worktree active')
        const changed = changes.find((c) => c.session_id === session.id && c.worktree)
        assert.equal(changed?.worktree?.state, 'active')
        assert.equal(changed?.worktree?.id, worktreeId)
        assert.equal(session.summary.worktree?.state, 'active')
        assert.equal(session.worktree?.id, worktreeId) // 其余字段保持

        // 没带 worktree 的 session.changed（比如只是标题变了）不会把它清掉
        await session.rename('renamed')
        assert.equal(session.worktree?.state, 'active')

        // git 事件按现有方式透传，类型来自 @anycon/protocol
        const { events } = await session.history()
        const git = events.find((e) => e.event.type === 'git')?.event
        assert.ok(git?.type === 'git')
        assert.equal(git.action, 'worktree_created')
        assert.equal(git.branch, 'feature/login')
        assert.equal(git.base, 'origin/main')

        // 重新 get 也能拿到
        const again = await node.sessions.get(session.id)
        assert.equal(again.worktree?.state, 'active')
        const [listed] = (await node.sessions.list()).filter((s) => s.id === session.id)
        assert.equal(listed?.worktree?.branch, 'feature/login')
      })
    })

    it('sessions.create：只带 base 时分支由 daemon 起名；创建被拒的原因（not_worktree_ready / exists / bad_base / scratch）', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const s = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          workspace: { mode: 'worktree' },
        })
        assert.deepEqual(paramsOf(env, 'session.create')[0]?.workspace, { mode: 'worktree' })
        assert.match(s.worktree?.branch ?? '', /^anycon\//)

        await rejectsRpc(
          node.sessions.create({
            agent: 'mock',
            project: 'plain',
            workspace: { mode: 'worktree' },
          }),
          'invalid',
          'not_worktree_ready',
        )
        await rejectsRpc(
          node.sessions.create({
            agent: 'mock',
            project: 'app',
            workspace: { mode: 'worktree', branch: 'main' },
          }),
          'exists',
        )
        await rejectsRpc(
          node.sessions.create({
            agent: 'mock',
            project: 'app',
            workspace: { mode: 'worktree', base: 'bogus' },
          }),
          'invalid',
          'bad_base',
        )
        await rejectsRpc(
          node.sessions.create({ agent: 'mock', workspace: { mode: 'worktree' } }),
          'invalid',
        )
      })
    })

    it('sessions.create：prompt 时 SDK 生成 uuid v4 的 message_id；第一个 turn 在 worktree 建好之后开始，用 history / events 取回', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const session = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          prompt: 'fix the login redirect',
          workspace: { mode: 'worktree', base: 'origin/main' },
        })
        const [params] = paramsOf(env, 'session.create')
        assert.equal(params?.prompt, 'fix the login redirect')
        assert.match(String(params?.message_id), UUID_V4)
        assert.deepEqual(params?.workspace, { mode: 'worktree', base: 'origin/main' })
        // 两次 create 的 message_id 不同
        await node.sessions.create({ agent: 'mock', project: 'app', prompt: 'again' })
        assert.notEqual(paramsOf(env, 'session.create')[1]?.message_id, params?.message_id)

        // 第一个 turn 跑完：git(worktree_created) 在 user_message 之前，user_message 带的就是 SDK 生成的 id
        const rec = env.daemon.sessions.get(session.id)
        await until(
          () => rec?.events.some((e) => e.event.type === 'turn_done') ?? false,
          3000,
          'first turn done',
        )
        const { events } = await session.history()
        const types = events.map((e) => e.event.type)
        assert.ok(types.indexOf('git') >= 0 && types.indexOf('git') < types.indexOf('user_message'))
        const user = events.find((e) => e.event.type === 'user_message')?.event
        assert.ok(user?.type === 'user_message')
        assert.equal(user.message_id, params?.message_id)
        assert.equal(user.content, 'fix the login redirect')
        assert.ok(types.includes('turn_done'))
      })
    })

    it('sessions.create：events({ fromSeq: 0 }) 从头取回第一个 turn，seq 严格递增、没有重复', async () => {
      await withNode(await impl.load(), async (node) => {
        const session = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          prompt: 'hello there',
          workspace: { mode: 'worktree' },
        })
        const seen: StreamEvent[] = []
        const abort = new AbortController()
        for await (const ev of session.events({ fromSeq: 0, signal: abort.signal })) {
          seen.push(ev)
          if (ev.type === 'turn_done') break
        }
        const seqs = seen.map((e) => e.seq)
        assert.deepEqual(
          seqs,
          [...new Set(seqs)].toSorted((a, b) => a - b),
          'no duplicates, ascending',
        )
        assert.equal(seen[0]?.type, 'git')
        assert.ok(seen.some((e) => e.type === 'user_message' && e.content === 'hello there'))
      })
    })

    it('events({ fromSeq })：补历史期间又到的实时事件不重复、不乱序（第一个 turn 的事件在 watch 之后、history 返回之前继续产生）', async () => {
      await withNode(await impl.load(), async (node, env) => {
        env.daemon.historyDelayMs = 80
        const session = await node.sessions.create({ agent: 'mock', project: 'app', prompt: 'hi' })
        const seqs: number[] = []
        for await (const ev of session.events({ fromSeq: 0 })) {
          seqs.push(ev.seq)
          if (ev.type === 'turn_done') break
        }
        assert.ok(seqs.length >= 6, `got ${JSON.stringify(seqs)}`)
        assert.deepEqual(
          seqs,
          seqs.map((_, i) => i + 1),
          'seq 1..N，每个一次，按序',
        )
      })
    })

    it('sessions.create：messageId 覆盖默认；同一个 messageId 重试拿到第一次创建的会话', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const first = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          prompt: 'once',
          messageId: 'create-key-1',
        })
        assert.equal(paramsOf(env, 'session.create')[0]?.message_id, 'create-key-1')
        const retry = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          prompt: 'once',
          messageId: 'create-key-1',
        })
        assert.equal(retry.id, first.id)
        assert.equal(env.daemon.sessions.size, 1)
        // 没有 prompt 时 messageId 被忽略
        await node.sessions.create({ agent: 'mock', project: 'app', messageId: 'ignored' })
        const last = paramsOf(env, 'session.create').at(-1)
        assert.ok(!('message_id' in last!) && !('prompt' in last!))
      })
    })

    it('gitStatus：includePr 才发 include_pr；worktree 会话带 base / ahead / behind，本地会话没有；scratch 会话 not_git', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const wt = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          workspace: { mode: 'worktree', branch: 'anycon/fix' },
        })
        const plain = await wt.gitStatus()
        assert.deepEqual(paramsOf(env, 'session.git_status'), [{ session_id: wt.id }])
        assert.equal(plain.branch, 'anycon/fix')
        assert.equal(plain.base, 'origin/main')
        assert.equal(plain.ahead, 2)
        assert.equal(plain.behind, 1)
        assert.equal(plain.uncommitted, 3)
        assert.equal(plain.conflicts, 0)
        assert.equal(plain.pr, undefined)

        const withPr = await wt.gitStatus({ includePr: true })
        assert.deepEqual(paramsOf(env, 'session.git_status')[1], {
          session_id: wt.id,
          include_pr: true,
        })
        assert.equal(withPr.pr?.number, 42)
        assert.equal(withPr.pr?.state, 'open')
        assert.equal(withPr.pr?.checks_pending, 1)

        await wt.gitStatus({ includePr: false })
        assert.deepEqual(paramsOf(env, 'session.git_status')[2], {
          session_id: wt.id,
          include_pr: false,
        })

        const local = await node.sessions.create({ agent: 'mock', project: 'app' })
        const status = await local.gitStatus()
        assert.equal(status.branch, 'main')
        assert.equal(status.base, undefined)
        assert.equal(status.ahead, undefined)

        const scratch = await node.sessions.create({ agent: 'mock' })
        await rejectsRpc(scratch.gitStatus(), 'invalid', 'not_git')
      })
    })

    it('diff：默认只发 session_id；against / path 原样发出；带 path 才有 patch；path 不在列表里被拒', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const s = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          workspace: { mode: 'worktree' },
        })
        const list = await s.diff()
        assert.deepEqual(paramsOf(env, 'session.diff'), [{ session_id: s.id }])
        assert.equal(list.from_sha, 'c'.repeat(40))
        assert.deepEqual(
          list.files.map((f) => [f.path, f.status, f.additions, f.deletions, f.untracked ?? false]),
          [
            ['src/a.ts', 'modified', 10, 2, false],
            ['new.txt', 'added', 1, 0, true],
          ],
        )
        assert.ok(list.files.every((f) => f.patch === undefined))

        const uncommitted = await s.diff({ against: 'uncommitted' })
        assert.deepEqual(paramsOf(env, 'session.diff')[1], {
          session_id: s.id,
          against: 'uncommitted',
        })
        assert.equal(uncommitted.from_sha, 'b'.repeat(40))
        assert.deepEqual(
          uncommitted.files.map((f) => f.path),
          ['new.txt'],
        )

        const one = await s.diff({ path: 'src/a.ts' })
        assert.deepEqual(paramsOf(env, 'session.diff')[2], { session_id: s.id, path: 'src/a.ts' })
        assert.equal(one.files.length, 1)
        assert.match(one.files[0]?.patch ?? '', /^diff --git a\/src\/a\.ts b\/src\/a\.ts\n/)

        await s.diff({ against: 'base', path: 'new.txt' })
        assert.deepEqual(paramsOf(env, 'session.diff')[3], {
          session_id: s.id,
          against: 'base',
          path: 'new.txt',
        })
        await rejectsRpc(s.diff({ path: 'not/in/the/list.ts' }), 'invalid')
      })
    })

    it('syncBase：默认只发 session_id；abort 原样发出；合并 / 冲突结果和各种失败原因', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const s = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          workspace: { mode: 'worktree' },
        })
        // 默认：已是最新
        assert.deepEqual(await s.syncBase(), { outcome: 'up_to_date', commits: 0 })
        assert.deepEqual(paramsOf(env, 'session.sync_base'), [{ session_id: s.id }])

        // 合并了 2 个提交
        env.daemon.syncResults.push({ outcome: 'merged', commits: 2 })
        const merged = await s.syncBase()
        assert.equal(merged.outcome, 'merged')
        assert.equal(merged.commits, 2)

        // 冲突：冲突文件列表
        env.daemon.syncResults.push({
          outcome: 'conflicts',
          commits: 3,
          conflicts: ['src/a.ts', 'README.md'],
        })
        const conflicted = await s.syncBase()
        assert.equal(conflicted.outcome, 'conflicts')
        assert.deepEqual(conflicted.conflicts, ['src/a.ts', 'README.md'])

        // 放弃合并
        const aborted = await s.syncBase({ abort: true })
        assert.deepEqual(aborted, { outcome: 'aborted', commits: 0 })
        assert.deepEqual(paramsOf(env, 'session.sync_base').at(-1), {
          session_id: s.id,
          abort: true,
        })

        // 失败原因：busy / uncommitted_changes / merge_in_progress / fetch_failed
        env.daemon.syncResults.push(
          AnyconRpcError.of('busy', 'a turn is running'),
          AnyconRpcError.of('invalid', 'uncommitted changes', { reason: 'uncommitted_changes' }),
          AnyconRpcError.of('invalid', 'merge in progress', { reason: 'merge_in_progress' }),
          AnyconRpcError.of('internal', 'fetch failed: no route', { reason: 'fetch_failed' }),
        )
        await rejectsRpc(s.syncBase(), 'busy')
        await rejectsRpc(s.syncBase(), 'invalid', 'uncommitted_changes')
        await rejectsRpc(s.syncBase(), 'invalid', 'merge_in_progress')
        const fetchFailed = await rejectsRpc(s.syncBase(), 'internal', 'fetch_failed')
        assert.match(fetchFailed.message, /no route/)

        // 没有 worktree 的会话
        const local = await node.sessions.create({ agent: 'mock', project: 'app' })
        await rejectsRpc(local.syncBase(), 'invalid')
      })
    })

    it('archive / delete：不带参数和以前一样（只发 session_id）；带 worktree 处置时原样发出，worktree 状态随 session.changed 更新', async () => {
      await withNode(await impl.load(), async (node, env) => {
        // 没有 worktree 的会话：行为不变
        const plain = await node.sessions.create({ agent: 'mock', project: 'app' })
        await plain.archive()
        await plain.unarchive()
        assert.deepEqual(paramsOf(env, 'session.archive'), [{ session_id: plain.id }])
        await plain.delete()
        assert.deepEqual(paramsOf(env, 'session.delete'), [{ session_id: plain.id }])

        // worktree 会话：archive({ worktree: 'keep' }) → kept
        const kept = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          workspace: { mode: 'worktree' },
        })
        await until(() => kept.worktree?.state === 'active', 3000, 'active')
        await kept.archive({ worktree: 'keep' })
        assert.deepEqual(paramsOf(env, 'session.archive')[1], {
          session_id: kept.id,
          worktree: 'keep',
        })
        await until(() => kept.worktree?.state === 'kept', 3000, 'kept')

        // archive({ worktree: 'auto' }) / archive({}) 都是合法的
        const auto = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          workspace: { mode: 'worktree' },
        })
        await auto.archive({ worktree: 'auto' })
        await auto.archive({})
        assert.deepEqual(paramsOf(env, 'session.archive')[2], {
          session_id: auto.id,
          worktree: 'auto',
        })
        assert.deepEqual(paramsOf(env, 'session.archive')[3], { session_id: auto.id })
        await until(() => auto.worktree?.state === 'removed', 3000, 'removed')

        // delete 的 worktree / deleteBranch（驼峰 → delete_branch）
        const gone = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          workspace: { mode: 'worktree' },
        })
        const deletedNotice = new Promise<SessionChanged>((resolve) => {
          const off = node.on('session_changed', (c) => {
            if (c.session_id === gone.id && c.deleted) {
              off()
              resolve(c)
            }
          })
        })
        await gone.delete({ worktree: 'remove', deleteBranch: true })
        assert.deepEqual(paramsOf(env, 'session.delete')[1], {
          session_id: gone.id,
          worktree: 'remove',
          delete_branch: true,
        })
        assert.equal((await deletedNotice).deleted, true)
        await rejectsRpc(node.sessions.get(gone.id), 'not_found')

        await kept.delete({ deleteBranch: false })
        assert.deepEqual(paramsOf(env, 'session.delete')[2], {
          session_id: kept.id,
          delete_branch: false,
        })
      })
    })

    it('syncBase：已归档的会话 invalid / archived', async () => {
      await withNode(await impl.load(), async (node) => {
        const s = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          workspace: { mode: 'worktree' },
        })
        await s.archive({ worktree: 'keep' })
        await rejectsRpc(s.syncBase(), 'invalid', 'archived')
        await s.unarchive()
        assert.equal((await s.syncBase()).outcome, 'up_to_date')
      })
    })

    it('projects.branches：参数是 project，返回 default_base / local / remote', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const branches = await node.projects.branches('app')
        assert.deepEqual(paramsOf(env, 'workspace.branches'), [{ project: 'app' }])
        assert.equal(branches.current, 'main')
        assert.equal(branches.default_base, 'origin/main')
        assert.deepEqual(branches.local, ['main', 'dev'])
        assert.deepEqual(branches.remote, ['origin/main', 'origin/dev'])
        assert.equal(branches.truncated, undefined)
        await rejectsRpc(node.projects.branches('plain'), 'invalid', 'not_git')
      })
    })

    it('worktrees.list / remove：project 过滤、force / deleteBranch 原样发出、各种拒绝原因', async () => {
      await withNode(await impl.load(), async (node, env) => {
        assert.deepEqual(await node.worktrees.list(), [])
        assert.deepEqual(paramsOf(env, 'workspace.worktrees'), [{}])

        const a = await node.sessions.create({
          agent: 'mock',
          project: 'app',
          workspace: { mode: 'worktree', branch: 'anycon/a' },
        })
        const b = await node.sessions.create({
          agent: 'mock',
          project: 'other',
          workspace: { mode: 'worktree', branch: 'anycon/b' },
        })
        await until(() => a.worktree?.state === 'active' && b.worktree?.state === 'active')

        const all = await node.worktrees.list()
        assert.deepEqual(all.map((w) => w.branch).toSorted(), ['anycon/a', 'anycon/b'])
        const onlyApp = await node.worktrees.list({ project: 'app' })
        assert.deepEqual(paramsOf(env, 'workspace.worktrees').at(-1), { project: 'app' })
        assert.equal(onlyApp.length, 1)
        const [info] = onlyApp
        assert.equal(info?.id, a.worktree?.id)
        assert.equal(info?.project, 'app')
        assert.equal(info?.state, 'active')
        assert.equal(info?.session_id, a.id)

        // 会话还没归档 → busy
        await rejectsRpc(node.worktrees.remove(a.worktree!.id), 'busy')
        assert.deepEqual(paramsOf(env, 'workspace.remove_worktree')[0], { id: a.worktree!.id })

        // 归档并保留；会话删掉之后 worktree 还在，只是没有会话
        await a.archive({ worktree: 'keep' })
        env.daemon.worktrees.set(a.worktree!.id, {
          ...env.daemon.worktrees.get(a.worktree!.id)!,
          uncommitted: 2,
        })
        const [kept] = await node.worktrees.list({ project: 'app' })
        assert.equal(kept?.state, 'kept')
        assert.equal(kept?.uncommitted, 2)

        // 有未提交改动而没 force → invalid / uncommitted_changes；force + deleteBranch 才删
        await rejectsRpc(node.worktrees.remove(a.worktree!.id), 'invalid', 'uncommitted_changes')
        await rejectsRpc(
          node.worktrees.remove(a.worktree!.id, { force: false }),
          'invalid',
          'uncommitted_changes',
        )
        assert.deepEqual(paramsOf(env, 'workspace.remove_worktree')[2], {
          id: a.worktree!.id,
          force: false,
        })
        await node.worktrees.remove(a.worktree!.id, { force: true, deleteBranch: true })
        assert.deepEqual(paramsOf(env, 'workspace.remove_worktree')[3], {
          id: a.worktree!.id,
          force: true,
          delete_branch: true,
        })
        assert.deepEqual(
          (await node.worktrees.list()).map((w) => w.branch),
          ['anycon/b'],
        )
        await rejectsRpc(node.worktrees.remove(a.worktree!.id), 'not_found')
      })
    })
  })
}
