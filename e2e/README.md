# e2e

Playwright 端到端测试：真实 hub + daemon（mock agent）+ web。只测 chromium。

## 跑

```bash
just gen-wasm && bun run --cwd web build     # 先 web：同时产出 web/dist 和 web/dist-preview，hub 编译时才会选中它们（见下）
cargo build -p anycon-hub -p anycon-cli      # 二进制放在 target/debug/
just e2e                                     # = bun install + playwright install chromium + playwright test
```

`global-setup.ts` 起下面这些（都用临时目录，**不碰真实的 `~/.anycon`**；另外在它自己的进程里起一个预览用的上游，见表后）：

| 服务 | 怎么起 |
|---|---|
| hub | `target/debug/anycon-hub`，**随机空闲端口**（`E2E_HUB_PORT` 可固定），`ANYCON_PUBLIC_URL=http://127.0.0.1:<port>`，数据目录 `$TMP/hub`，hub 自己的模块开 debug 日志（用例要断言日志里没有短码），管理员 token 由 globalSetup 生成并经 `ANYCON_ADMIN_TOKEN` 交给 hub（测试里 `readState().adminToken`） |
| SMTP | `helpers/smtp.ts` 的最小 SMTP 服务（Node `net`，在 globalSetup 的进程里，随机空闲端口），收到的信**落成文件**（`$TMP/smtp/<序号>.json`；用例在 worker 进程里，所以靠读目录取信：`readState().smtpDir`，`readMails` / `mailsTo` / `waitForCode`）。hub 的 `ANYCON_SMTP_URL=smtp+plain://127.0.0.1:<port>`、`ANYCON_EMAIL_FROM='Anycon <noreply@example.test>'` 指向它（ADR-37）：登录页多了邮箱验证码登录，**token 表单折叠在「使用 token」（`login-use-token`）后面**——`helpers/ui.ts` 的 `login` / `submitLogin` 会先展开（`revealTokenForm`） |
| daemon | `ANYCON_HOME=$TMP/daemon ANYCON_MOCK_AGENT=1 anycon init --hub … --workspace $TMP/workspace --name e2e-node --yes`，再 `anycon config set agents.claude-code.enabled false`（本机装了真 Claude Code 也不让它抢默认 agent），然后 `anycon daemon`（`ANYCON_AGENTS__ENV_FROM_LOGIN_SHELL=false` 跳过登录 shell 环境解析，socket 立刻就绪；globalSetup 等 `daemon.sock` 出现）。daemon 的环境里 `GIT_CONFIG_GLOBAL=$TMP/gitconfig`（`user.name` / `user.email` / `init.defaultBranch = main`）、`GIT_CONFIG_NOSYSTEM=1`：它跑的 git（每会话 worktree 的合并提交等）不读开发机的 git 配置；`readState().gitConfig` 是这份文件，测试里的 git 命令用同一份（`helpers/git.ts`）。daemon 的 `PATH` 最前面是一个 `git` 包装脚本（`$TMP/git-shim/git`）：平时原样 `exec` 真的 git，只有 `readState().slowGitDir` 里有规则文件 `<项目目录名>.<n>.rule`（内容 `<秒数> <参数里要含的字符串>`）时，那个目录里参数匹配的第一条 git 才先睡（每条规则一次性）。测试用 `slowGit()`（`helpers/git.ts`）写规则，稳定地造出「git 元数据算得慢」 |
| web | 默认由 hub 嵌入的 `web/dist` 提供（hub 的 `build.rs` 在**编译时**选 `web/dist` 和 `web/dist-preview`，所以 web 要先于 hub 构建；之后只重建 web 不用重编 hub，`touch crates/hub/build.rs` 可强制重选）（baseURL = hub 地址）；`E2E_WEB_MODE=preview` 用 `vite preview`（随机端口，`E2E_WEB_PORT` 可固定）；`E2E_WEB_URL=http://127.0.0.1:5173` 指向已有的 dev server |

**dev server 预览（ADR-39）的前提**：
- hub 带 `ANYCON_PREVIEW_URL=http://*.preview.localhost:<hub 端口>`。Chromium 自己把 `*.localhost` 解析到回环地址（Node 的 `fetch` 不一定会，所以 Node 侧要访问预览域名时连 127.0.0.1、只改 `Host` 头：`helpers/preview.ts` 的 `fetchPreviewHost`），`*.localhost` 也算安全上下文，service worker 能用。`ANYCON_PUBLIC_URL` 仍是 `http://127.0.0.1:<端口>`（IP，hub 不做「同一个可注册域名」检查）
- 预览运行时是 `web/dist-preview`，和 `web/dist` 由同一条 `bun run --cwd web build` 产出（单独构建它：`cd web && bunx vite build -c vite.preview.config.ts`）。hub 的 `build.rs` 在**编译时**选定它（目录不存在就嵌入占位页「The preview runtime is not built into this hub」），所以**构建顺序是 wasm → web → hub**；先编了 hub、后构建 web 的话 `touch crates/hub/build.rs` 再编。debug 构建下 hub 读磁盘，目录已存在时之后只重建预览运行时不用重编 hub。`global-setup` 启动后会核对预览域名上的引导页就是 `web/dist-preview/index.html`，不一致直接报错并说明怎么修（`E2E_ALLOW_STALE_WEB=1` 忽略）；`E2E_BUILD=1` 把这几步都做了
- 上游：`helpers/upstream.ts`，globalSetup 进程里的一个最小 dev server 替身（Node `http`，WebSocket 回显手写，没有额外依赖），监听 127.0.0.1 的随机端口，端口在 `readState().upstreamPort`。daemon 的 `preview.allow_ports = [<上游端口>]`（`anycon config set`，在 daemon 启动之前写）：它不属于任何会话，靠配置放行。页面 / 脚本 / 接口的清单见文件顶部
- Web UI 要由 hub 提供（默认）：预览页的 `frame-ancestors` 只允许 hub 自己的 origin，`E2E_WEB_MODE=preview` / `E2E_WEB_URL` 下 iframe 嵌不进去，预览用例会跳过

端口默认随机，所以可以同时跑多份、不怕 18080 之类的固定端口被占；随机出来的端口写回 `process.env`，Playwright 的 worker 继承，状态文件按端口命名（`.e2e-state-<port>.json`），Playwright 的 `outputDir` 也按端口分目录（`test-results/<port>`；它启动时会清空 outputDir，共用一个目录会互相删掉对方的 trace）。环境变量全表见 `helpers/env.ts`。常用：

- `E2E_SKIP_DAEMON=1` 只起 hub + web（smoke 够用；happy path 会被跳过）
- `E2E_REQUIRE_DAEMON=0` daemon 起不来只警告（默认是整体失败，happy path 依赖它）
- `E2E_KEEP_TMP=1` 结束后保留临时目录（`hub.log` / `daemon.log` 在里面）
- `E2E_TMP_DIR_FILE=<文件>` 把临时目录的路径写进这个文件（CI 用它在失败时把 `hub.log` / `daemon.log` 收进产物）
- `E2E_BUILD=1` 先构建 web（`web/dist` + `web/dist-preview`）、触碰 `crates/hub/build.rs`、`cargo build -p anycon-hub -p anycon-cli` 再跑（hub 编译时要选中这两个目录；debug 构建读磁盘，所以之后只重建 web 也行）。没设时，hub 服务的 `index.html` 和当前 `web/dist/index.html` 不一致（占位页 / 旧的嵌入），或预览域名上的引导页和 `web/dist-preview/index.html` 不一致，会直接报错（`E2E_ALLOW_STALE_WEB=1` 忽略）

## 用例

- `tests/smoke.spec.ts` — hub `/healthz`；登录页邮箱输入在前、`login-token-input` 折叠在 `login-use-token` 后面（展开后可见）
- `tests/email-login.spec.ts` — 邮箱验证码登录（ADR-37），串行，真 hub + 假 SMTP：1. 账号设置里绑定邮箱（`email-link` → 输入 → 假 SMTP 收到绑定码 → 先填错码：hub 回 `400 invalid_code`，页面只提示「码不对」、不被当成登录失效踢回登录页 → 再填对的，`email-code` 输满自动提交 → `email-current`）；2. 新的浏览器上下文（`locale: 'zh-CN'`：登录页是中文，hub 按 `Accept-Language` 发中文邮件，UTF-8 经 lettre 编码，用例要解码才取得到码）在登录页用邮箱（大小写混合，hub 规范化成小写）+ 码登录：先输错一次（显示「验证码不对或已过期」、输入清空、`/api/me` 仍是 401），再用同一封信里的码登录，会话属于绑定了这个邮箱的账号，同一个码不能用第二次；3. 没绑定的邮箱：「已发出」提示和绑定了的**一字不差**，假 SMTP 一封都没收到，随便什么码都进不去；4. 解绑；5. hub 的日志（debug 级）里没有邮箱原文和验证码，只有邮箱哈希前 8 位的标记（去掉每行开头的时间戳再找，它的微秒部分是 6 位数字）。限速：hub 的发码每 IP 每小时 5 次（登录和绑定各一个桶，没有环境变量），同一邮箱同一用途 60 秒一次；这个文件登录发码 2 次、绑定发码 1 次（`notifications.spec.ts` 用另一个地址再绑定 1 次），所以**不要在同一个 hub 上 `--repeat-each` 它**（每次运行 global-setup 起新 hub，不受影响）。
- `tests/enroll.spec.ts` — 账号内接入（ADR-36），串行；一个已配对的浏览器（owner）贯穿整个文件，每个用例再开全新的、存储隔离的上下文（visitor）登录同一个账号，不用短码解锁节点。**按文件名排在 happy-path 之前，所以整套一起跑时它先把 daemon 配对到账号上**（happy-path 第 1 步兼容两种顺序）：
  - a. owner 在节点设置 →「接入」里设节点口令；visitor 登录后 `/` 被导到 `/unlock/<node>`，扫码入口默认展开但不自动开始（`unlock-trigger-scan` 展开着，只有 `unlock-show-qr` 按钮、没有二维码），点了才出二维码（`unlock-qr`，只断言出现，e2e 读不了二维码）；页面进后台（脚本模拟 `visibilitychange`）二维码作废、出现 `unlock-scan-paused`，回到前台不自动重开，要再点 `unlock-show-qr`；然后换到「节点口令」入口、改设备名、用口令解锁，进节点首页；daemon 的设备列表里有这台（改过的名字）
  - b. 错口令：`unlock-error[data-code=wrong_secret]`，`anycon passwd --status --json` 里失败 1 次；再输对的进去，失败计数清零
  - c. owner「添加设备」→「接入码」页拿 6 位码（`enroll-code`）；visitor 点「我有接入码」输入后解锁
  - d. visitor 点「在已配对设备上批准」→「发送请求」；owner 出现横幅 `status-banner[data-kind=enroll_request]` →「查看」→ 批准对话框 → 批准 → 读出 6 位码（`approve-code`）；visitor 输入后解锁；owner 的卡片收尾（`approve-resolved`）、横幅收起
  - e. 同上，owner 拒绝：visitor 看到 `unlock-error[data-code=denied]`
  - f. 扫码批准：visitor 先点 `unlock-show-qr`，出现的二维码（元素截图）放到 owner 的「假摄像头」前——`installFakeCamera` 注入的 `enumerateDevices` / `getUserMedia`（canvas 流，画 `window.e2eFakeQr`），其余照旧走真的 `<video>` → 解码；owner 的「设备」页「扫码批准」认出二维码 → 确认页 → 批准，visitor 不输入任何东西直接解锁。web 生成的二维码内容（base32、node / enroll id、密钥 r）第一次和 Rust daemon 真的对上
  - g. 已经配对了节点 A 的浏览器，在账号里的另一台节点 B 上用口令解锁：测试自己起第二个 daemon（`startExtraDaemon`）、用另一个浏览器把它配对进账号、在 B 上设口令；只认识 A 的浏览器从节点切换器里带锁的 B（`node-locked`）进 `/unlock/<B>`，用现有设备身份在现有连接上发起（不开临时连接），hub 里不新建设备（`/api/devices` 的 web 设备 id 集合不变）、只多一条 `paired_with`，daemon B 认这台设备；结束时把节点 B 从账号里去掉（happy-path 第 8 步假定账号下只有一台节点）
  - h. owner 的「接入」标签里「最近活动」有记录；口令（对的和错的）不在 hub / daemon 的日志里
  限速：hub 对 `enroll_begin` 按 IP 限速（令牌桶，容量 10、每 6 秒回 1 个），所有 visitor 都来自 127.0.0.1，而解锁页打开时不会自动开始接入，只有用户点了的入口（扫码入口要点 `unlock-show-qr`）才算一次，所以用例里每个 visitor 打开之前先按它最多会点开始的接入次数从本地模拟的同一个桶里预订（`reserveEnrollBegins`），不够就等桶回填；整个文件登录得很快，结束时还要 `loginHeadroom(5)`（`helpers/ui.ts`，本地模拟登录限速桶）等桶回填一点，免得后面文件里裸点「登录」撞 429；超限时 hub 的回执还会被每秒一条的预算吞掉，页面要等 20 秒才报「超时」（见 docs/spec/web.md § 4.3.1，20 秒无回应）。结束时 `anycon passwd --clear --yes` 清掉节点口令。
- `tests/happy-path.spec.ts` — 串行 9 步（一个浏览器上下文，第 8、9 步再开新的、存储隔离的）：
  1. 管理员 token 登录 → 账号下没有 daemon 时 `/setup`（单独跑；整套一起跑时 enroll.spec 已经配对过，落到 `/unlock/<node>`，测试再手动进 `/setup`）；2. `anycon pair --json` 取短码（链接里的短码在 fragment：`/pair#code=…`）、输入、配对成功（跳过可选的「设置节点口令」一步 `setup-pw-skip` 和「要不要开通知」一步 `setup-notify-skip`，后者在 Chromium 上会问：127.0.0.1 是安全上下文、hub 配了推送）、
  3. 新建项目；4. 新建会话表单里切模型（`mock-small` 再回 `mock-large`）/ 思考（`low`）/ 权限模式（`plan` 再回 `ask`）；
  5. 发 `run permission`，出现权限卡片，点允许；6. `tool-block`、`turn-usage`，回显 `[model=mock-large thinking=low mode=ask]`；
  7. 刷新页面历史仍在（IndexedDB + `session.history`）；
  8. 第二个上下文登录（这个浏览器没有已配对的节点、账号下有 daemon，落到 `/unlock/<node>`，ADR-36；以前是 `/setup`），直接去 `/pair` 用短码配对为第二台设备（`e2e-second-browser`），第一台再发 `run permission`，在第二台上答复，第一台卡片变为「Allowed by e2e-second-browser」。
  9. 配对链接：第三个上下文直接打开 `anycon pair --json` 给的 `link`（`/pair#code=…`，短码在 fragment），先登录，登录后出现确认步骤（从链接带进来的短码不会自动开始配对），点确认后配对，节点设置 → 设备里有三台浏览器；hub 日志里没有短码。
- `tests/lifecycle.spec.ts` — 独立用例，每个自己配对自己的浏览器（`helpers/ui.ts` 的 `pairNewBrowser`），互不依赖：
  - a. **8 位短码（`--short`）**：`anycon pair --short --json` 给 `XXXX-XXXX`（默认是 16 位，happy path 第 2 步用默认），输入短码配对；daemon 的已配对列表里有这台设备，hub 日志里没有短码
  - b. **吊销后重新配对**：A、B 两台浏览器；A 在节点设置 → 设备里吊销 B；B 出现「需要重新配对」（横幅 `hub_revoked` 或节点页 `noise-failed`）；B 点重新配对、用新短码配对成功，回到在线；daemon 里是新的设备 id
  - c. **退出并移除本设备**：账号菜单 → 退出并移除本设备；跳回登录页，IndexedDB 所有库所有 store 都是空的；daemon（`anycon devices --json`）和 hub（`/api/devices`）两侧都看不到这台设备
  - d. **大工具输出**：mock 的 `big` 关键词发一个 300 KB 的工具输出；连发 4 次（总量超过一页 192 KiB），断言 daemon 把输出截断（保留 `BIGSTART` … `BIGEND`，中间有 `…[truncated N bytes]`）、刷新后仍能加载、一台全新设备（空缓存）靠线上的 `session.history` 翻页也能加载全部 4 个 turn，hub / daemon 日志里没有 too_large / 解密失败
  - e. **页面在后台**：脚本模拟 `visibilitychange`（Playwright 的无头页面始终是 visible）。hidden 8 秒再回来：hub 里这台设备（`GET /api/devices` 的 `online`）一直在线、节点一直 ready；hidden 满 30 秒（用例里断言不早于 25 秒）：hub 看到它离线——此后 daemon 的 `notify` 才会走 Web Push；断开期间 CLI（`anycon new … --no-wait`）新建一个会话，页面收不到；回到 visible：自动重连、设备又在线、节点 ready，并补同步——新会话出现在列表里，不用刷新。约 45 秒
- `tests/linked-project.spec.ts` — 关联本机已有目录（ADR-33），每个用例自己配对：
  - a. 新建项目 →「已有目录」→ 选择器里粘贴路径、选当前目录，名称默认取目录名，关联后项目页标「已关联」+ 路径；在里面发 `run permission`，权限卡片上的 cwd 就是那个目录；`/`（`link_roots` 之外）只给行内错误；隐藏目录打开「显示隐藏目录」后才列出，且不能关联（`data-blocked="hidden"`）
  - b. 从列表移除：项目消失，目录和里面的文件都还在
  - c. 目录被删掉：项目页显示「目录不存在」、没有输入框，确认后移除
  - d. 关联后把目录换成指向数据目录的符号链接：项目变成「目录不存在」，不能开会话
  - e. 权限请求挂着时把目录换掉再点允许：按拒绝处理（`permission-settled` 或 `data-by="daemon"`），工具不执行
  global-setup 把 `workspace.link_roots` 设成 `<tmpDir>/external`（默认 `~` 不含系统临时目录），并关掉 `claude-code` / `codex`（它们没有配置段也参与探测，本机装了真的会成为默认 agent）
- `tests/image-attachments.spec.ts` — 图片附件（ADR-35），两台浏览器各自配对（happy path 第 9 步因此只断言「至少三台、含第二台」）：
  - 新建 scratch 会话时用 `composer-file-input` 选一张 PNG（测试里现场生成），缩略图 `composer-attachment[data-state=ready]` 后带文字发送：mock 回复里的 `Attachments: red.png (image/png, <字节数> bytes, sha256 <前 12 位>)` 和本地算的一致（证明 daemon 把原文件交给了 agent），用户气泡里有 `message-attachment` 且图片解码成功（`blob:` 地址）
  - 只发图片不带文字；粘贴一张（构造带文件的 `paste` 事件）+ 再选一张，一条消息两个附件，回显顺序不变
  - 清掉 IndexedDB 的 `blobs` 缓存后刷新、以及第二台空缓存的设备打开同一会话：历史里的 4 张图经 `blob.get` 拉回来；点开是 `attachment-viewer`；hub 日志里没有文件名
- `tests/notifications.spec.ts` — 通知（ADR-37），串行，一个配对好的浏览器（`channel: 'chromium'` = 新 headless，只有它的 `context.grantPermissions(['notifications'])` 生效；`headless_shell` 授予不了）。Chromium 没有真的推送服务，所以只给 `PushManager.subscribe` / `getSubscription` 打桩（`addInitScript`，订阅记在 localStorage 里、刷新后还在）：返回 endpoint 在 hub 白名单里（`https://fcm.googleapis.com/fcm/send/e2e-…`）、`p256dh`（P-256 未压缩点，65 字节）/ `auth`（16 字节）合法的假订阅；权限、service worker、`POST /api/push/subscriptions`、hub 的校验和存储都是真的。**不点「发送测试推送」，也不让 daemon 给这台设备发 `notify`**（否则 hub 会真的去连 Google）；结束时订阅、邮箱、偏好都还原。1. 配对过节点、浏览器能收推送、还没订阅：`status-banner[data-kind=notifications]`，点 `banner-notifications-enable` 进 `/settings/notifications`，之后（刷新也）不再出现，localStorage 里 `notifyBannerDismissed`；2. 偏好开关：账号还没绑邮箱时邮件列禁用并说明要先绑定（`notify-email-why[data-why=bind]`）；绑定（用 `e2e-notify@example.test`，API）后改一个 push 开关和一个 email 开关，刷新后还在，hub 的 `GET /api/me/notifications` 一致，再还原；3. `push-enable`：`push-status[data-state=on]`，`GET /api/push/subscriptions` 里恰好一条属于这台设备（`endpoint_host = fcm.googleapis.com`，endpoint 本身不回显），页面订阅时用的 `applicationServerKey` 就是 `/api/config.vapid_public_key`，刷新后仍是 on 且不重复上报（行 id 不变）；4. `push-disable`：浏览器退订，hub 里没有这台设备的订阅；5. `/setup` 向导里「开启通知」；6. 普通「退出登录」（`logout` → `logout-confirm`，确认框里写着推送会关掉）也退订推送：浏览器里的订阅（桩的 localStorage 标记）没了，换一个登录的请求上下文看 hub 里这个账号没有订阅行，重新登录后是同一台设备（设备 id 不变）、推送关着。
- `tests/preview.spec.ts` — dev server 预览（ADR-39），真实 hub（配了 `ANYCON_PREVIEW_URL`）+ daemon + 浏览器 + 上面的上游；每个用例自己配对浏览器、新建 scratch 会话、切到「预览」（`main-view-preview`）、用「其他端口」（`preview-port` 选「其他端口…」→ `preview-other-port` → `preview-other-open`）填上游端口：
  - a. iframe（`preview-frame`）里出现上游的页面（标题、正文，页面脚本看到的 `location.origin` 是预览 origin）、同源 WebSocket 经 shim 走隧道的回显、约 600 KiB 的脚本（上游 gzip，跨很多帧）完整到达；子协议原样回来；iframe 的 sandbox 不含 `allow-top-navigation`；hub 的日志里没有票据密钥、令牌、票据 id，也没有预览的内容，daemon 的日志里没有密钥和令牌
  - b. cookie：先访问设 cookie 的接口（一个 HttpOnly、一个不是），再请求回显接口，上游收到的 `Cookie` 头里两个都有；`document.cookie` 只看得到非 HttpOnly 的；页面自己设的 cookie 也随之后的请求发出；刷新后页面脚本读到的 cookie 写进 DOM；浏览器原生的 cookie 里什么都没有（否则会发给 hub）。上游收到的请求头：`host` 是 `localhost:<上游端口>`、带 `accept-encoding: gzip`、没有 `x-forwarded-*`。320 KB 的请求体经隧道原样往返；上游不压缩的大脚本由 daemon 压缩，字节和 sha256 一致
  - c. 地址栏输入路径回车跳转；上游的 302（`location` 指向 `http://localhost:<上游端口>/landed`）被改写，iframe 和页面里的 `fetch` 都停在预览 origin 内；粘贴整条 URL 只取路径
  - d. 「在新标签页打开」（`context.waitForEvent('page')`）：新页面的 origin 是 `http://<label>.preview.localhost:<hub 端口>`、从地址栏里的路径开始、内容正确，地址栏里没有 fragment（启动参数被 `replaceState` 抹掉）
  - e. 「停止预览」：`preview-stopped`、iframe 撤掉；别处开着的预览（新标签页）显示「这个预览已被停止 / 回到 Anycon 重新打开」（隧道收到 `GOAWAY { closed }` 要一点时间，所以刷新到出现说明页为止）；「重新开始」（`preview-restart`）后同一个 label 又能用
  - f. 节点设置的「预览」标签（`tab-preview`）列出票据（端口、会话、签发设备、过期、隧道数），「关闭」→ 确认 → `preview-list-empty`；另一个窗口里 iframe 的下一次导航显示结束页
  - g. 安全回归：Web UI 的 origin 上 `/__anycon/sw.js`、`shim.js`、`config.json` 都是 404、`/__anycon/enter` 回的是 Web UI 自己的页面、没有注册预览的 service worker；预览域名上 `/__anycon/sw.js` 带 `Service-Worker-Allowed`、引导页的 CSP 带 `frame-ancestors <hub origin>`、`/api/*` 回引导页、`POST /auth/token` 是 405、label 格式不对是 404；预览页（sandbox 的 iframe）里 `top.location = …` 被拒（`SecurityError`）、`window.open(…, '_top')` 不带走 Web UI、读不了父页面
  机器忙时首屏（引导页 → 加载 wasm → service worker → Noise 握手 → 取页面）会慢到十几秒甚至更久，用例里 iframe 里的等待上限是 `CONNECT_TIMEOUT`（45 秒）；这是对一个具体条件的有界等待，不是固定睡眠。
- `tests/worktree.spec.ts` — 每会话 worktree（ADR-34），真实 git；每个用例自己配对浏览器、在 workspace 根下自己建一个仓库项目（`.gitignore` + `.worktreeinclude` + 被忽略的 `.env`，带一次提交）：
  - a. 主流程：新建会话选「新 worktree」→ 发 `write` → 分支 chip 变成 `active`、`.env` 被复制进 `<项目>/.anycon/worktrees/<名字>/`、主目录里没有 `anycon-mock.txt` →「改动」标签里有它（`added` + `untracked`）和展开的 diff → 在 worktree 里提交、在主目录的 `main` 上再提交一次 →「同步 base」`sync-result[data-outcome="merged"]` + 消息流里的 `git-event`，磁盘上是合并提交（提交人来自临时 gitconfig）→ 再 `write`（已跟踪文件的修改，「未提交」视图里是 `modified`）→ 概览里归档（自动）→ 因为有未提交改动 worktree 被保留 → 节点设置 → Workspace → Worktree 里是「已保留」→ 勾「丢弃未提交的改动」删除 → 目录不在了、分支还在
  - b. 「当前目录」模式：没有 worktree，「改动」标签只有「未提交」视图（没有「相对 base」分段、没有「同步 base」），`anycon-mock.txt` 是未跟踪文件；归档不弹 worktree 的确认框
  - c. 非 git 项目没有「工作位置」一行（先是占位，补拉回来仍没有 `git` 才当非 git 项目，所以先等占位消失再断言；也没有分支 chip 和「改动」标签）；没有提交的仓库（`worktree_ready = false`）这一行禁用并有 `new-session-workspace-disabled` 说明，会话照常在项目目录里工作
  - d. base 下拉选 `release`（比 `main` 少一个提交）、自定义分支名：不合法的名字挡住发送，worktree 的内容来自 `release`，同名分支再建一次失败（「分支已存在」、文字放回输入框）
  - e. 干净的 worktree 自动归档：目录删了、分支（有自己的提交）留着 → 取消归档按分支重建，`.worktreeinclude` 的文件复制回来，agent 在重建的 worktree 里继续写
  - f. 同步 base 遇到冲突：`conflicts`、冲突横幅和文件、消息流里 `git` 事件上的「让 agent 解决 / 放弃合并」；「让 agent 解决」发出快捷消息（带分支和 base，**不列冲突文件名**：断言消息里没有 `shared.txt`），「放弃合并」后回到合并之前（`MERGE_HEAD` 没了）
  - g. 删除会话：选「删除 worktree」并勾「同时删除分支」（旁边提示会丢提交）→ 目录和分支都没了，节点设置里没有这一行
  - h. 删除会话时选「保留 worktree」：它留在节点设置里（`worktree-no-session`），在那里删除并同时删分支，行也没了
  - i. worktree 目录被删：会话标 `missing`，下一条消息按分支重建（`worktree_restored`），提交过的改动都在
  - j. worktree 目录被换成指向 daemon 数据目录的符号链接：`missing`，发消息得到 `not_found`，不重建、不跟随链接（和 linked-project d / e 一样的换目录攻击）
  - k. 冷缓存 + git 元数据算得慢：`slowGit` 让 daemon 为这个项目算 git 元数据时约 9 秒（`symbolic-ref` 睡过单条 git 的 5 秒硬超时，daemon 退到串行的 `rev-parse --short HEAD`，它再睡 4 秒）。浏览器连上时的第一次 `workspace.list` 等满 3 秒的冷缓存预算，拿不到 `git`；打开新建会话表单先是占位 `new-session-workspace-pending`（发送键禁用、`composer-blocked` 说明），网页补拉一次（又等 3 秒）也没有，占位仍在（`git_pending`）；约 9 秒后 daemon 发 `workspace.changed { updated }`，「工作位置」自己出现（输入框还是同一个 DOM 节点、草稿还在），默认「新 worktree」，发送后真的建了 worktree（`.env` 复制进来、主目录没有 `anycon-mock.txt`）
  - k2. 同样的慢 git（约 9 秒，比前台 3 秒 + 补拉 3 秒还慢）：daemon 对该项目一直给 `git_pending: true`，占位 `new-session-workspace-pending` 一直撑到 `workspace.changed { updated }` 之后；用例每 100 ms 采样，断言这段时间占位一直在、发送键一直禁用，最后分段控件出现、占位消失、发送可点（以前补拉超时后有约 2.5 秒既没占位也没分段控件、发送放行，会悄悄建成「当前目录」）
  - l. 已归档的 worktree 会话：自动归档（干净的 worktree 被删，分支留着）后页面没有 `composer` / `composer-input` / `composer-send`，有 `session-archived-bar`、`session-archived-hint`、`session-unarchive`（刷新后仍然）；`anycon send <会话> … --no-wait` 被 daemon 拒绝（退出码 2、stderr 提示 `anycon unarchive`），worktree 没有被重建；点 `session-unarchive` → 输入框回来、`git-event[worktree_restored]`、`.env` 复制回来；再发 `write` 写进重建的 worktree（`anycon-mock.txt` 第 2 行、`M`），主目录里没有
  - m. daemon 刚重启 + 一批仓库项目同时冷缓存：共享的 daemon 属于 global-setup、整套用例共用，不能重启，所以这个用例自己起一个节点（`helpers/daemon.ts` 的 `startExtraDaemon`：自己的 `ANYCON_HOME` 和 workspace、mock agent，`finally` 里停掉），建 12 个带提交的仓库项目，**重启**它（进程换掉、缓存清空），再让新浏览器配对上去；打开最后一个项目的新建会话表单，「工作位置」分段控件一定出现，发送后会话建在 worktree 里，回首页再开第一个项目也有这一行
  - n. 等审批时不能归档：`run permission` 的会话停在 `waiting_permission`，信息面板里 `session-archive` 禁用、旁边 `session-archive-blocked` 写明先停止或答复（强行点也不弹归档确认框）；`anycon archive <id>` 被 daemon 拒绝（`busy: a turn is in progress or waiting for an answer`），turn 还在等、worktree 还在、没有 `session-archived-bar`；允许之后 turn 结束，说明消失、按钮可用，归档成功
  - o. 项目改名：先归档再取消归档让会话没有 agent 进程（空闲的进程也算“在跑”，改名会 `busy`；空闲 10 分钟才收），然后 `anycon projects rename`（不 await）；`slowGit` 让改名后目录里的 `git worktree repair` 睡 5 秒，目录一换名就 `anycon send` → `busy`；改名返回后 worktree 登记指向新目录（`git worktree list` 里是新路径），马上从网页继续发 `write`：写进搬了家的 worktree、chip 仍 `active`、没有 `worktree_missing`、没有多出 `worktree_restored`
  **不预热 daemon 的 git 缓存**：每个用例在配对浏览器之前才建仓库项目，浏览器连上时拉的第一次项目列表就是这个项目的冷缓存（daemon 对冷缓存的项目最多等 3 秒，算不完的先不带 `git`，算完之后发 `workspace.changed { updated }`；网页在这段时间显示占位并挡住发送，web.md § 4.5）。所以新建会话的流程是：等 `new-session-workspace` 分段控件出现（`waitForWorkspacePicker`，最多 45 秒）再选 / 发，**不在占位期间点发送**；非 git 项目要等占位消失（补拉回来仍没有 `git` 才当非 git 项目）再断言没有这一行。机器忙时的验证：`bunx playwright test tests/worktree.spec.ts --repeat-each=3`，同时起 12 个 `perl -e '1 while 1'` 占满 CPU（跑完杀掉）
  mock agent 的行为见 `crates/adapters/src/mock/README.md`。UI 文案中英都可能出现（默认 en-US），断言用正则兼容

等待的约定：没有固定睡眠。全部是对 data-testid 的显式等待、`expect.poll`，或对一个具体条件的有界轮询（`helpers/process.ts` 的 `waitFor` / `waitForLog`；等 daemon 连上 hub 用 `anycon stats --json` 的 `hub_connected`）。

测试里读运行状态：`readState()`（`helpers/state.ts`）；建仓库项目、提交、让 daemon 的 git 变慢：`makeRepoProject` / `commitFile` / `git` / `slowGit`（`helpers/git.ts`，git 带临时配置和超时；`waitForProjectGit` 是用 `anycon projects --json` 轮询到项目有 `git` 元数据的预热，worktree 用例已经不用了）；跑一条应当失败的 CLI 命令看退出码和 stderr：`runCliResult`（`helpers/cli.ts`）；用例自己拥有的第二个 daemon（重启之类）：`startExtraDaemon`（`helpers/daemon.ts`）；让 daemon 发起配对：`startPairing(state, { short? })`（`helpers/cli.ts`，解析 `anycon pair --json [--short]` 的第一行 `{ code, link, expires_at_ms }`，并校验短码长度、链接里的 `#code=`）；开一个已配对的新浏览器：`pairNewBrowser(browser, state, { deviceName, short?, contextOptions?, prepare? })`（`helpers/ui.ts`；`contextOptions` / `prepare` 用来给新上下文加权限、打桩）；不经页面改账号的邮箱 / 读推送订阅：`bindEmailViaApi` / `unbindEmailViaApi` / `listPushSubscriptions` / `deleteAllPushSubscriptions` / `webDeviceId`（`helpers/account.ts`）；假 SMTP：`startSmtp`（global-setup 用）、`readMails` / `mailsTo` / `waitForCode` / `mailText`（`helpers/smtp.ts`）。

## 浏览器

`bunx playwright install chromium`（`just e2e` 会做）。在需要代理的机器上设 `HTTPS_PROXY`；
CDN 不通时可设 `PLAYWRIGHT_DOWNLOAD_HOST` 指向镜像。CI 用 `--with-deps`。
