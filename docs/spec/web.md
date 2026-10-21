# Web UI 功能规格

> 细化 [architecture.md § 2.3](../architecture.md#23-客户端) 和 [roadmap.md](../roadmap.md) 的 Web UI 条目。路由、布局、每个页面的内容、模型 / 思考 / 权限模式的选择方式、状态层和缓存。组件级视觉规范另写 `web/DESIGN.md`。

## 1. 定位

**手机上能用，桌面上好用。** 典型场景是在手机上给家里的机器发一句话、过十分钟回来看结果、点一下审批。桌面上则是开着一个会话长时间跟进。

**给开发者用，体验对齐 Claude 桌面版**（[ADR-34](../decisions.md#adr-34-web-ui-是开发者客户端体验对齐-claude-桌面版每个会话可以有自己的-worktree)）：登录、配对、项目、会话、审批、设置之外，每个会话可以在自己的 git worktree 和分支里工作，会话页能看相对 base 的改动、同步 base、让 agent 提交和开 PR、看 PR 状态。hub 只管连通性，这些都经 E2E 走 daemon。

不做：终端、远程 shell、编辑器（diff 只读，改代码交给 agent）、经 hub 预览本地 dev server。

## 2. 路由

TanStack Router，文件式路由。`:node` 是 daemon 的 DeviceId，URL 里用前 8 位短 id，冲突时回退全长。

| 路由 | 页面 | 需要 |
|---|---|---|
| `/login` | 登录 | 无 |
| `/invite/:code` | 邀请落地：存 code 到 sessionStorage，转 `/login` | 无 |
| `/auth/callback/:provider` | OAuth 回调由 hub 处理后 302 到 `/`，这个路由只在 hub 回错误时展示 | 无 |
| `/` | 重定向：这个浏览器有已配对的节点 → 上次打开的 `/n/:node`；没有，但账号下有节点 → `/unlock`；账号下也没有 → `/setup` | 登录 |
| `/setup` | 首次向导：安装 daemon → 配对 | 登录 |
| `/pair` | 输入短码配对新节点；支持配对链接 `/pair#code=<短码>`（短码在 URL fragment，不是 query） | 登录 |
| `/unlock` | 账号下、这个浏览器还没配对的节点列表（§ 4.3.1） | 登录 |
| `/unlock/:node` | 用扫码批准、接入码或节点口令解锁一台节点（§ 4.3.1） | 登录 |
| `/n/:node` | 节点首页：项目列表、临时会话、最近会话 | 登录 + 配对 |
| `/n/:node/p/:project` | 项目页：该项目的会话历史和管理（新建会话去 `/n/:node/new`） | 同上 |
| `/n/:node/s/:session` | 会话页（主界面） | 同上 |
| `/n/:node/new` | 新建会话（空白页，不显示历史），`?project=&agent=`。左栏项目行的「＋」、项目页的「新会话」都到这里；独立路由也方便从通知和命令面板直达 | 同上 |
| `/n/:node/files/*` | 只读文件浏览（v0.2） | 同上 |
| `/n/:node/settings` | 节点设置：agent、设备、接入、workspace、限制、关于。`?tab=<agent\|devices\|enroll\|workspace\|limits\|about>` 直达某个标签；推送和安全提醒点进来用 `?tab=enroll`，`enroll_requested` 另带 `review=1`，自动弹出待批准请求的对话框 | 同上 |
| `/settings` | 账号设置，子路由见 § 4.9 | 登录 |
| `/admin` | 管理：邀请、账号、hub 状态 | 管理员 |

本账号的节点、只是这个浏览器还没配对 → 跳到 `/unlock/:node`（从通知、书签、另一台设备发来的链接进来时常见）；不是本账号的节点、已吊销的节点 → 404 页带「返回」；节点离线不是 404，正常进入页面并显示离线状态。

## 3. 布局

### 3.1 桌面（≥ 1200px）

```
┌──────────┬──────────────────────────────────────┬────────────┐
│ 节点切换  │ 面包屑  节点 / 项目 / 会话标题  状态   │ 会话信息    │
│──────────│──────────────────────────────────────│            │
│ 搜索 ⌘K  │                                      │ agent      │
│ + 新会话  │                                      │ 模型       │
│          │           消息流                      │ 思考       │
│ 项目 A ＋│                                      │ 权限模式   │
│  · 会话1 │                                      │ 用量 / 费用 │
│  · 会话2 │                                      │────────────│
│ 项目 B ＋│  ╭────────────────────────────────╮  │ 待审批 (1) │
│ 临时会话  │  │ ＋ 输入框               ⏹  发送 │  │ 改动的文件  │
│          │  ╰────────────────────────────────╯  │  src/a.ts  │
│──────────│  [权限▾][更多]        [模型▾][思考▾] │  …         │
│ ● 在线   │                                      │            │
│ 账号 ▾   │                                      │            │
└──────────┴──────────────────────────────────────┴────────────┘
   260px 可收起           自适应                    320px 可收起
```

- 左栏：节点切换器（顶部，下拉；多节点时显示在线状态点；账号下这个浏览器还没解锁的节点列在下拉底部，带锁图标，点了去 `/unlock/:node`）、搜索、新建按钮、按项目分组的会话树（最近活跃的项目在前，每个项目默认展开最近 5 个会话；项目行右边一个「＋」，在这个项目里新建会话）、临时会话分组、底部连接状态和账号菜单。左栏可收起（Logo 旁的按钮或 `⌘B`，收起后页面头部最左边出现展开按钮），收起状态记在 localStorage。宽屏时拖动左栏右缘调整宽度（默认 260px，200–480px，最宽不超过半个窗口），双击右缘恢复默认，键盘聚焦后 ← / → 调整；宽度也记在 localStorage
- 中栏：会话。只有一行顶栏：面包屑可点，最后一级是会话标题、点击编辑；右侧依次是「聊天 / 改动」开关（带图标，「改动」后面是未提交文件数）、危险模式 chip、状态 chip（运行中 / 等待审批 / 空闲 / 已结束 / 节点离线）、节点状态、「ⓘ」
- 右栏：会话信息面板，可收起，收起状态记在 localStorage

### 3.2 平板（768–1199px）

两栏：左栏 + 中栏，右栏变成从右侧滑出的抽屉，由头部的「ⓘ」按钮打开。

### 3.3 手机（< 768px）

单栏，栈式导航：

```
节点首页 → 项目页 → 会话页
   ↑                    ↓ 左滑 / 返回
左侧抽屉（节点切换、项目树、设置入口）
```

- 会话页占满屏幕，头部只留返回、标题、状态 chip、「ⓘ」
- 输入区固定底部，`100dvh` + `env(safe-area-inset-bottom)`，iOS 键盘弹起时输入框跟随
- 选项（模型 / 思考 / 权限；新建会话还有项目、工作位置、agent）收进输入框里左侧一个「⚙」按钮，点开底部弹层
- 待审批请求以底部横幅出现，不遮消息

### 3.4 状态横幅（全局）

在头部下方，按优先级只显示一条：

1. 和 hub 断开，正在重连（红）
2. 节点离线，消息会留在本地（黄）
3. 有设备请求接入某台节点（蓝，带「查看」，打开批准对话框，§ 4.3.2；请求结束就消失）
4. 有 N 条未发送消息（黄，带「现在重发」）
5. 该浏览器尚未启用通知（蓝，带「去开启」→ `/settings/notifications`，可关闭，只提示一次）。同时满足才显示：浏览器能收推送（§ 4.9 的能力检测）、hub 配了推送（`/api/config.push_available`）、这个浏览器配对过节点、本浏览器没有推送订阅、浏览器没有拒绝过通知、`/settings/notifications` 以外的页面（在那一页上不催）、用户没关过。点「去开启」也算提示过了；关闭（或点了「去开启」）记在 localStorage 的偏好 `notifyBannerDismissed` 里，之后不再显示。没有注册 service worker（开发服务器）时也不显示
6. 新版本 Web UI 已就绪，刷新生效（蓝）

## 4. 页面

### 4.1 登录 `/login`

- 从 `GET /api/config` 拿启用的登录方式，没启用的不显示
- 布局顺序：passkey（如果浏览器支持且 https）→ GitHub / Google 按钮 → 邮箱 → token（折叠在「使用 token」链接后，只在 `single`）
- 局域网 http 访问时，passkey 和 OAuth 按钮显示但禁用，hover 说明「需要通过 https 域名访问」
- 邮箱两步（`/api/config.login_methods` 里有 `email` 才显示，这时 token 表单折叠在「使用 token」后面；只有 token 时直接显示 token 表单。[ADR-37](../decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)）：输入邮箱 → `POST /auth/email/send` → 输入 6 位码（`inputmode=numeric`、`autocomplete="one-time-code"`，输满自动提交）→ `POST /auth/email/verify`。发码后显示「如果这个邮箱绑定了账号，验证码已经发出」（hub 不告诉你邮箱有没有绑定），按 hub 回的 `resend_after_secs`（缺省 60 秒）倒计时后才能重发；码不对显示「验证码不对或已过期」。一句提示：「邮箱要先在账号设置里绑定才能用来登录」
- 邀请：从 sessionStorage 读 `invite`，用 `/auth/invite` 兑换（邮箱不能注册账号）
- passkey 使用 conditional UI：页面加载即发起 `navigator.credentials.get({ mediation: 'conditional' })`，有 passkey 的用户点一下自动填充就进去

### 4.2 首次向导 `/setup`

账号下一台节点都没有时的落地页（账号下有节点、只是这个浏览器没配对时去 `/unlock`，§ 4.3.1）。三步，每步可跳过：

1. **装 daemon**：按检测到的平台给命令（macOS brew、Linux 安装脚本、「其他」），`anycon init --hub <当前 hub 地址>` 已填好 hub 地址，一键复制
2. **配对**：告诉用户在那台机器上跑 `anycon pair`，下面是短码输入框（同 `/pair`）
3. **完成**：配对成功后先问要不要给这台节点设口令（「出门在外、手边没有已配对设备时，用它在任何设备上解锁这台节点」，`node.set_enroll_password`，可跳过，之后在节点设置的「接入」里设），再问要不要给这个浏览器开通知（「开启通知」直接走 § 4.9 通知页同一套：权限提示 → 订阅 → 上报 hub；可跳过，之后在 设置 → 通知 里开。这个浏览器收不了推送、hub 没配推送、通知已被浏览器拒绝或已经开着时不问，直接往下；读状态期间只显示一行「正在检查…」，不闪一下问题），然后跳到 `/n/:node`

### 4.3 配对 `/pair`

- 输入框接受 **16 位或 8 位**短码（`anycon pair` 默认给 16 位，`--short` 给 8 位），自动大写、每 4 位插入连字符、忽略粘贴进来的连字符和空格；整条配对链接粘贴进来也认（取其中 `#code=` 后面的）。确认后先做一次 Argon2id 派生（浏览器里约 0.1–1 s，进度显示「正在派生密钥…」），再发 `pair_join`
- **配对链接** `https://<hub>/pair#code=<短码>`（二维码里就是它）：短码在 URL **fragment** 里，浏览器不会把它发给 hub，hub 和反代的日志里也没有。页面从 `location.hash` 读它，读完立刻把它从地址栏和历史记录里擦掉（短码只在内存里；旧的 `?code=` 形式还能读，同样擦掉）。登录态没有时先去登录：跳转前把短码放进 `sessionStorage`，**登录页的 URL（path / query / hash）和历史栈里都不能出现短码**——否则用户在登录页刷新一次，短码就进了 hub 和反代的访问日志；登录成功回到 `/pair` 时取出并立刻删除
- **从链接带进来的短码不会自动开始配对**：先显示一个确认步骤（「用这个短码配对这个浏览器？」+ 确认按钮），点了才开始。原因：攻击者可以拿一台全新的 daemon 生成链接发给已登录的人，自动开始会把攻击者的节点静默加进受害者的账号
- 进度：连接 hub → 派生密钥（Argon2id，约 0.1–1 s）→ 撮合 → 握手 → 与节点确认 → 完成。「与节点确认」就是等 daemon 经新建立的加密传输发来的 `pair.ack` 并校验（见 § 6.4），每步有超时提示
- **落盘时机**：节点只有在 `pair.ack` 校验通过之后才写进 IndexedDB 和 store。中途任何一步失败，都不留下半成品
- 失败分支：短码不存在或过期 / 频率限制 / 握手失败（PSK 不对，几乎只在手输错时发生，提示重试）/ 配额 / **确认失败**（30 秒内没收到 `pair.ack`，或 `pair.ack` 里的 id、指纹与 hub 告知的不一致：不保存任何东西，提示重试，并提醒核对 hub 地址是否可信）/ **公钥冲突**（本浏览器里已有同 id 的节点，但公钥不同：不覆盖，提示先「忘记这个节点」再配对）
- 设备名默认 `浏览器 · 系统 · origin 主机名`，完成前可改

### 4.3.1 解锁节点 `/unlock`

[ADR-36](../decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)，消息序列见 [protocol.md § 4.4](../protocol.md#44-账号内接入)。账号下有节点、只是这个浏览器还没和它配对时用（换了新手机、借来的电脑、清过浏览器数据），不需要短码。

**列表 `/unlock`**：`GET /api/devices` 里 `kind = daemon`、未吊销、本浏览器 IndexedDB 里没有的节点，每行：名称、平台、在线状态、最近在线。这些名称来自 hub、**未经认证**，只用来挑节点；解锁完成后以 `pair.ack` 里的名字为准。只有一台时直接进 `/unlock/:node`。底部「用短码配对」→ `/pair`。

**`/unlock/:node`**：标题「解锁 <节点名>」和一句说明「这个浏览器还没和这台节点配对。选一种方式证明是你：」。节点离线时所有入口禁用，说明「解锁需要节点在线」（在线状态来自 `/api/devices`，节点上线后自动恢复）。四个入口，同一时刻只进行一个：

| 入口 | 适合 | 流程 |
|---|---|---|
| **用已配对的设备扫码**（默认展开） | 手机在手边 | 点「显示二维码」才开始（打开页面不自动开始，免得忘在后台的标签页一直占着 daemon 的接入名额和 hub 的限速）：发 `enroll_begin` 和 `EnrollHello { method: scan }`，页面显示二维码（内容 `anycon-enroll:1:…`）和 5 分钟倒计时，下面一句「在手机上打开 anycon，点账号菜单里的「扫码批准新设备」」。二维码在收到 `reply { wait_approval }` 之后才出现，只在内存里，不写 IndexedDB、不进 URL。到期、和 hub 的连接断了、或者二维码正显示着时改了设备名，都自动换一个新的（连续换 12 次后停下，显示「重新开始」；页面在后台时到期不换）；还没人批准时标签页切到后台就取消这次接入（批准之后的握手让它走完），回到前台显示「重新显示二维码」，不自动重开。手机批准后自动往下走 |
| **在已配对设备上批准** | 手机在手边，但不方便扫码 | `EnrollHello { method: code, approval: true }`。页面显示「已经向你的已配对设备发了请求。在那台设备上点批准，然后在这里输入它显示的 6 位码」，下面是 6 位输入框（`inputmode=numeric`、`autocomplete="one-time-code"`，输满自动提交），剩余次数显示在输入框下面 |
| **我有接入码** | 已经在已配对设备的「添加设备」里，或用 `anycon pair --enroll` 拿到了码 | `EnrollHello { method: code, approval: false }`，同一个 6 位输入框 |
| **节点口令** | 手边没有任何已配对设备 | `EnrollHello { method: password }`。口令输入框（`type=password`），旁边一句「在借来的电脑上，不要让浏览器保存口令」；anycon 自己从不保存口令。提交后先显示「正在派生密钥…」（Argon2id，§ 6.5） |

- **进度**：连接 hub → 发起接入 →（等待批准）→ 验证 → 握手 → 与节点确认 → 完成。「与节点确认」就是 `pair.ack` 校验，规则、落盘时机、公钥冲突的处理和 `/pair` 完全一样（§ 4.3、§ 6.4）
- **连接**：这个浏览器在这个 origin 上还没有 device token 时，开一条升级请求带登录 cookie 的临时连接；已经是这个 hub 上的设备（配对过别的节点）时，在现有连接上发 `enroll_begin`。成功后按 `pair_done` 原地升级 / 加入节点，然后照常走 IK
- **设备名**：默认同 `/pair`，作为 `EnrollHello.hint` 发出去（让批准的人认得出来），完成前可改
- **失败分支**（`EnrollReply` 的 `rejected.reason` 和 `enroll_failed.reason`）：
  - `wrong_secret`：码 →「码不对」和剩余次数，用完显示「这个码已作废，重新请求」；口令 →「口令不对」
  - `backoff`：「尝试太多次了，<倒计时> 后再试」，期间提交按钮禁用
  - `locked`：「这台节点的口令解锁已被锁定（连续输错 10 次）。在节点本机运行 `anycon passwd --enable`，或在已配对设备的节点设置「接入」里重新启用」
  - `password_not_set`、`method_disabled`：「这台节点没有开启这种方式」，引导到其他入口；`no_code`：「这台节点现在没有有效的接入码，在已配对设备上重新生成」
  - `denied`：「请求被拒绝了」；`expired`、`timeout`：「超时了」+「重新开始」；发出 `enroll_begin` 后 20 秒没有任何回应（hub 的限速回执可能被每秒一条的预算丢掉）：「没有收到 hub 的回应，可能是请求太频繁，稍后再试」，只能手动「重新开始」，扫码入口也不会自己重试（接入码入口的 `expired` 说「码已过期或次数用完」）
  - `offline`、`busy`（「这台节点同时有太多接入在进行」）、`rate_limited`、`quota`：各一句说明
  - `not_found`（不是本账号的节点）：404 页
- 离开页面、切换入口时发 `enroll_cancel`

### 4.3.2 批准接入、应用内扫码

在**已配对**的浏览器上，处理别的设备发来的接入请求（ADR-36）。

**批准对话框**（新设备选了「在已配对设备上批准」）：收到 `node.enroll_requested` 时出现 § 3.4 的横幅，点「查看」打开：

```
有一台设备想接入 mac-mini
  Chrome · Windows（设备自报，未经验证）· 2 分钟前 · 还剩 3:12

只批准你眼前这台设备。
如果不是你刚刚在另一台设备上发起的，点「拒绝」。
                                         [拒绝]  [批准]
```

- **批准** → `node.enroll_approve { enroll_id }` → 对话框换成大号的 6 位码和倒计时，下面一句「在新设备上输入这个码。不要告诉任何人，anycon 不会向你索要它。」码只在这个对话框的组件状态里（不进 store、不进 IndexedDB），关掉就看不到了；新设备用掉或过期后对话框显示「已完成」/「已过期」
- **拒绝** → `node.enroll_deny { enroll_id }`
- 别的设备先处理了（`node.enroll_resolved`，或自己答复时得到 `already_resolved`）→ 收起成一行「已由 iPhone 批准 / 拒绝」
- hint 显示前清掉控制字符和双向控制符，按字符截到 64；没有就显示「未知设备」
- 打开页面或重连后用 `node.enroll_list` 补齐还在等的请求
- 节点设置「接入」标签的待批准列表里，`scan` 请求只能拒绝或去扫码（批准要扫到二维码里的密钥）

**应用内扫码**（新设备选了「用已配对的设备扫码」）：入口在账号菜单的「扫码批准新设备」和节点设置的「设备」标签。没有摄像头（`navigator.mediaDevices` 不可用或枚举不到视频输入）时不显示入口。

- 全屏对话框，`getUserMedia({ video: { facingMode: "environment" } })`。画面只在本地解码，不上传、不录制、不截图。优先用 `BarcodeDetector`，浏览器没有时用打包进来的 JS / wasm 解码器（CSP 只允许同源脚本，不能从 CDN 拉）
- 只认 `anycon-enroll:1:` 开头的内容，其他内容提示「这不是 anycon 的接入二维码」；二维码里的节点不是这个浏览器配对过的 →「这个浏览器没有和 <节点> 配对，不能批准」
- 认出来后立刻停掉摄像头。`node.enroll_list` 里这个请求不是 `scan` →「这个二维码对应的不是扫码请求」；是 `scan` 但不在等批准 →「这个请求现在不在等批准」。两种都清掉扫到的密钥，不进确认页。否则显示确认页：节点名、`node.enroll_list` 里这个请求的 hint（标「未经验证」）、一句「只在这个二维码就显示在你眼前那台设备上时批准」，按钮「批准」「取消」。确认后 `node.enroll_approve { enroll_id, secret }`，成功显示「已批准，新设备会自动完成接入」
- 摄像头权限被拒：说明怎么在浏览器设置里打开，或者让新设备改用「在已配对设备上批准」
- 扫到的密钥只在内存里，确认页关掉就丢弃

**安全提醒**：收到 `node.security_event` 时弹 toast，例如「新设备 X 已接入 mac-mini」「mac-mini 的节点口令连续输错 3 次」「mac-mini 的口令解锁已被锁定」「mac-mini 的节点口令已被 iPhone 修改」，点开去节点设置的「接入」标签。

### 4.4 节点首页 `/n/:node`

- 头部：节点名、在线状态、版本、「设置」
- **项目**：卡片或列表（可切换），每个显示名称、git 分支、是否有未提交改动、最近修改、进行中会话数。点击进项目页。右上「新建」→ 对话框三种方式：空项目（名称、是否 `git init`）/ 从 Git 克隆（URL、名称可选，进度条走 `job.progress`）/ **已有目录**（`workspace.browse` 驱动的目录选择器：面包屑 + 子目录列表，带 git 标记，已是项目的灰掉并标出项目名；`blocked` 非空的不能选，显示原因（`hidden` / `workspace` 的能进入；拒绝表和数据目录点进去会被 daemon 拒绝，显示行内错误）：隐藏目录（在 `workspace.link_roots` 里显式列出才能关联）/ 系统或凭证目录，或者包含它们 / 在 workspace 里面或包含它 / anycon 数据目录 / 文件系统根；也可以直接粘贴路径；名称默认取目录名，可改；提交 `workspace.link`）。关联项目的卡片标「已关联」并显示路径；菜单里是「从列表移除」（`workspace.unlink`，说明不会删除文件）而不是「移到回收站」；`missing` 的显示「目录不存在」，不能新建会话，只能移除。`workspace.browse` 返回 `forbidden`（`link_roots` 为空）时这一项显示为不可用并说明原因
- **临时会话**：scratch 会话列表，每条有「转为项目」
- **最近会话**：跨项目按时间倒序，带状态 chip
- 离线状态：项目列表显示上次缓存，灰化，顶部黄条；新建按钮禁用

### 4.5 项目页 `/n/:node/p/:project`

- 头部：项目名（可改名）、分支、路径、「…」菜单（重命名、移到回收站）
- 关联项目（`kind = linked`）：头部多一个「已关联」徽标；菜单里是「从列表移除」（`workspace.unlink`）**替代**「移到回收站」，确认框写明目录和文件不会被删除、已有会话保留——同一个目录重新关联回来就能继续，这个名字以后指向别的目录时旧会话不能继续；`busy`（还有运行中的会话）时留在确认框里显示原因、可以重试
- `missing`（关联目录不在了）：显示说明卡（带「从列表移除」），**不渲染新建会话的输入框**，菜单里只剩「从列表移除」；新建会话页的项目下拉里它是禁用的。节点首页上 missing 的卡片灰化、不显示修改时间，要进项目页才能移除
- 会话列表：筛选 全部 / 进行中 / 等待审批 / 已归档；每条显示标题、agent 图标、模型、最后一条消息摘要、时间；有 worktree 的会话再显示分支名（`⑂ anycon/x`）。左栏会话树里分支名放在悬停提示里
- 头部有「新会话」按钮（`project-new-session`，`missing` 时不显示），到 `/n/:node/new?project=<名字>`；项目页本身不再放输入框，只是会话历史和管理
- **新建会话页** `/n/:node/new`：空白页，中间一行标题（「想在 X 里做什么？」）和输入框，不显示任何历史（历史在左栏和项目页）。输入框和会话页的 composer 同一个组件；宽屏时输入框顶上贴一条上下文栏（`composer-context`，比输入框略窄、垫在它后面），放项目（下拉，含「临时会话」，`missing` 的项目禁用）和工作位置；下面的选项行左边是权限模式，右边是 agent（多于一个时）、模型、思考等级。发送后创建会话并跳转
- **工作位置**（新建会话页，ADR-34）：节点 `capabilities.worktree` 且项目是 git 仓库时，上下文栏里项目后面多一个按钮（`new-session-workspace-button`，「⑂ 新 worktree · main ▾」/「📁 当前目录 ▾」），点开是一个浮层（手机上直接在「⚙」底部弹层里）：
  ```
  工作位置 [ 新 worktree ][ 当前目录 ]
  基于     [main ▾]
  分支     [anycon/自动 ✎]
  ```
  - 分段控件（`Segmented fill`），`data-testid="new-session-workspace"`；选「新 worktree」时出现 base 下拉（`workspace.branches`：本地和远端两组，默认 `default_base`）和可选的分支名输入框（占位符 `<branch_prefix>/自动`，留空由 daemon 生成；输入时按 git 的分支名规则做基本校验）
  - 项目 `git_pending = true`（daemon 冷启动时 git 元数据还在算）：按钮的位置显示占位「正在读取 git 信息…」（`new-session-workspace-pending`），**一直到**收到 `workspace.changed { updated }`、重新拉到的项目不再 `git_pending` 为止；占位期间发送被挡住（按钮禁用并说明，回车也不发），不会悄悄按「当前目录」建会话。项目既没有 `git` 也没有 `git_pending`（旧 daemon）时，补拉一次 `workspace.list`（同一 `(节点, 项目)` 5 秒节流，请求超时 10 秒）期间同样占位，补拉回来仍没有就按非 git 项目处理。节点离线、节点不支持 worktree、临时会话、项目已有 `git` 时不占位
  - 初始选择：这个浏览器在这个 `(节点, 项目)` 上次的选择（localStorage，随 `lastUsed` 一起持久化），没有就用 `node.info.worktree.default_mode`
  - 项目是 git 仓库但 `worktree_ready = false`：按钮显示「当前目录」，浮层里的分段控件禁用并说明「项目目录不是仓库根目录，或者还没有提交」；非 git 项目、scratch 会话不显示这个按钮
  - 发送：这一行出现且可用时明确带 `workspace`（选「当前目录」就带 `{ mode: "local" }`），禁用 / 不显示时不带；`worktree_ready` 缺省（旧 daemon）按不可用处理。`session.create { …, workspace: { mode: "worktree", base, branch? } }`；创建后会话页头部先显示「正在创建 worktree…」，`git { action: worktree_created }` 之后变成分支 chip

### 4.6 会话页 `/n/:node/s/:session`

主界面。分三块：消息流、输入区、信息面板。

#### 消息流

事件 → 渲染的对应关系：

| 事件 | 渲染 |
|---|---|
| 用户消息 | 右对齐的浅灰底气泡；未发送的加「未发送」角标和重试 / 编辑 / 删除；附件显示成气泡上方的缩略图（按 `width` / `height` 先占位，进入视口才用 `blob.get` 拉，点开看大图），拉不到显示「图片不可用」 |
| `turn_start` | 一条细分隔线，hover 显示 turn 编号和时间 |
| `thinking_delta` | 「思考中…」一行灰字（不画卡片），流式时显示最近两行，结束后折叠成一行「思考了 12 秒」。可展开。设置里可改为始终展开或隐藏 |
| `text_delta` | Markdown 流式渲染（react-markdown + shiki），代码块有复制按钮和语言标签。**远程图片（`http(s)://`）不会自动加载**：被提示注入的 agent 可以借图片 URL 把数据带出去，所以只显示成带主机名的占位按钮，点了才请求；hub 的 CSP（`img-src 'self' data: blob:`）本来就不允许页面加载第三方图片，点了之后会退成一条链接「从 <主机> 打开」。`data:image/*`（非 SVG）、`blob:` 直接渲染；同源只放行已知静态资源路径（`/assets/<名字>`、`/icons/<名字>` 和几个固定的根文件：只有一层路径，名字限 `[A-Za-z0-9._-]`、≤ 128 字符，不能带 `?`、百分号编码或 userinfo），其他同源路径（比如 `/x?d=…`，能不点击就把内容送到 hub）同样走占位按钮。`javascript:` 等协议的链接被拦；**不开 `skipHtml`**——没有 rehype-raw，react-markdown 默认就把 HTML 当文本转义（开了会把 `Vec<String>` 里的 `<String>` 当标签吞掉） |
| `tool_call` 按 `category` | 收起时是一行灰字（图标 + 摘要 + ›，失败的是红字，不画卡片），展开的内容放在下面的框里；连着的思考 / 工具行之间只留很小的间距。`shell`：终端样式块，命令 + 输出，**默认收起**：一行摘要，agent 给了命令说明（Claude Code Bash 的 `input.description`）就显示说明，否则显示 `$ 命令`，放不下截断、悬停看全文；展开后先是折行显示的完整命令（右上角复制按钮，复制不带 `$ `），再是输出，输出超过 20 行再折叠；`file_edit`：文件路径 + diff 视图（react-diff-view），默认折叠显示 +N −M；`file_read` / `search`：一行摘要（「读取 src/a.ts」「搜索 "foo" → 3 个结果」），可展开；`web`：URL 一行；`mcp` / `subagent` / `other`：工具名 + 可展开的 JSON |
| `tool_output_delta` | 追加进对应 `shell` 块，边跑边显示 |
| `tool_result` | 合并进对应 `tool_call` 块，`is_error` 时块变红 |
| `plan` | 一张可折叠的待办卡片，随后续 `plan` 事件在**同一 turn 内**原地更新，不新增卡片；下一个 turn 另起一张（跨 turn 的话那张卡离当前太远） |
| `permission` (`kind = tool`) | 内嵌卡片：工具类别图标、摘要（命令全文 / 文件路径 / diff）、详情可展开、按钮**按 `options[]` 渲染**（常见是「允许」「本会话都允许」「拒绝」，按 `kind` 着色），`reject_*` 可附一句话。答复过的收成一行灰字「标题 — 已由 iPhone 允许（选项）›」，点开看命令 / diff；`by_device = "daemon"` 显示「已被 anycon 自动允许 / 拒绝 / 处理」（超时、取消、项目目录变了都会这样）；自己答复时得到 `not_found` 且 `data.reason = "project_dir_changed"`，卡片定格成一行说明「项目目录变了，这个请求已被拒绝，turn 已中止」，不再给按钮；其它 `not_found` 显示通用的「这个请求已经不存在」 |
| `permission` (`kind = plan`) | 计划卡片：渲染 `detail.text` 的 Markdown，按钮「开始实施」「继续规划」（可附修改意见）；按钮完全由 `options[]` 渲染，v0.1 没有「开始实施，回到之前的模式」 |
| `permission` (`kind = question`) | 问答卡片：每个问题一组单选 / 多选 / 文本框，一次提交全部 `answers` |
| `turn_done` | 一行：「复制」图标（复制这一轮 agent 的全部文字，Markdown，不含工具输出）+ 灰字：模型、用时、token 数、费用（agent 报了才显示） |
| `error` | 红色块，`recoverable` 时带「重试」 |
| `status` | 不单独渲染，驱动头部 chip 和 composer 状态 |
| `file_changed` | 不单独渲染，汇总到信息面板的「改动的文件」 |
| `settings_changed` | 一行灰字：「模型已切换为 …」 |
| `git` | 一行灰字（失败是红字，冲突 / worktree 被保留是黄字），按 `action`：「已在 worktree `anycon/x` 中开始（基于 main，复制了 2 个文件）」「worktree 创建失败：…」「已删除 worktree（分支保留 / 分支已删除）」「worktree 有未提交的改动，已保留」「已按分支重建 worktree」「worktree 目录不见了」「main 没有新提交」「已合并 main 的 3 个提交」「合并 main 时 2 个文件有冲突：a.rs、b.rs」+ 按钮「让 agent 解决」「放弃合并」「已放弃合并」。同时触发 git 状态刷新 |

- 列表虚拟化；初次进入拉最近 200 个事件，向上滚动再拉更早的（`session.history`）
- 自动跟随底部；用户向上滚动后停止跟随，出现一个圆形「↓」浮标（只有图标，「新消息」在 aria-label / 悬停提示里）
- 每条 assistant 消息 hover 出现「复制 Markdown」

#### 输入区（composer）

- 一个圆角框：上面是待发送图片的缩略图，下面一行是「＋」、输入框、停止 / 发送；框下面一行是选项（宽屏）
- 「＋」点开浮动菜单，目前只有「添加文件或照片」（见下面「图片附件」），以后的入口都加在这里
- 自动增高的 textarea，最多 10 行后内部滚动
- `Enter` 发送、`Shift+Enter` 换行（手机上 Enter 换行，发送靠按钮；设置里可改）
- 运行中：发送按钮变「停止」（`session.cancel`）；此时输入的内容会**排队**成下一条消息，并明确标注「将在本轮结束后发送」。agent 支持 turn 中插话的（capabilities.`interject`）则直接发
- 节点离线：输入框可用，发送后标「未发送」
- 归档：会话在运行或等审批时「归档」按钮禁用并说明「先停止或答复」（daemon 此时返回 `busy`）
- 已归档的会话：输入区（连同选项栏）换成一条「已归档 · 取消归档后才能发送消息」+「取消归档」按钮（`session-archived-bar` / `session-unarchive`），点了调 `session.unarchive`，成功后输入框回来；取消归档失败（`git_failed`）时说明「没能恢复会话的 worktree，会话仍是归档状态」。`send` 被 `invalid` / `archived` 拒绝时（别的设备刚归档），失败气泡和 toast 显示同一句说明，本地立刻把会话标成已归档，消息留着不丢；已归档会话的错误块不给「重试这一轮」
- 附件（ADR-35，见下面「图片附件」）：「＋」菜单选图、粘贴图片、拖进输入区；节点 `capabilities.blobs` 为真且 agent 的 `capabilities.attachments` 非空才显示
- 选项行（输入框下面）：左边权限模式和「更多」（其余 agent 特定选项），右边模型和思考等级，都是下拉（§ 5）；窄屏收进「⚙」底部弹层

#### 图片附件（ADR-35）

- **入口**：输入框左边「＋」（`composer-plus`）菜单里的「添加文件或照片」（`composer-attach`，打开 `accept` 为 agent 收的类型加 `image/*` 的文件选择框，可多选）、在输入框里粘贴图片、把图片拖进输入区（拖入时整块高亮）。非图片文件给一句提示后忽略。agent 不收附件时没有「＋」；节点不在线时菜单项禁用并说明原因
- **规整**：上传前在浏览器里处理（ADR-35「2026-10 修订」）——长边 > 2 000 px 的一律缩到 2 000；缩完（或原图）> 1.5 MiB 就有损重编码（agent 收 WebP 且浏览器编得出 → WebP 0.9，否则 JPEG 0.9，有透明通道的先铺白底）；单张硬上限 3.75 MiB；带 EXIF 的 JPEG 一律重编码（方向转正、去掉 GPS 等元数据）；浏览器能解码而 agent 不收的格式（HEIC、BMP…）转成 agent 收的格式。agent 收的格式、长边 ≤ 2 000、≤ 1.5 MiB、没有 EXIF 的原样上传（动图 GIF 保持动图）。一条消息的附件合计 ≤ 15 MiB，再加的图提示后忽略。解不开的图给提示。文件名去掉控制字符和双向控制符；下载时文件名用 `<主名>.<mime 对应扩展名>`
- **上传**：选中就开始传（`blob.put_begin` → 每块 512 KiB 的 `blob.put_chunk` → `blob.put_end`；同一节点同时只有 1 张在上传、2 块在途，其余排队（规整可以提前做最多 2 张）——同一设备到 daemon 是一条有序的传输，这样「允许」「停止」最多排在约 1.4 MB 后面；daemon 每台设备同时也只收 8 个进行中的上传），输入框里一排 64 px 缩略图（`composer-attachment`，`data-state` = `uploading` / `ready` / `error`），点开看大图（和消息里的图同一个对话框），上传中显示进度，失败的可以点重试，每张右上角有移除（移除时 `blob.discard`）。一条消息最多 10 张。有附件在上传时发送按钮禁用；上传完了，只有图片没有文字也能发
- **发送**：`session.send` / `session.create` 带 `attachments`；「未发送」的消息连同附件引用一起留在本地，重发照常（daemon 已回收的附件会得到 `not_found` / `blob`，「编辑」时从本地缓存重传）。新建会话失败后重发沿用第一次的 `message_id`（超时但其实建好了时 daemon 返回那个会话）。草稿里的附件引用随草稿持久化，刷新后用 `blob.get { max_bytes: 1 }` 探一下，已被回收的从缓存重传（缓存没有就提示并去掉）。新建表单换到不收图的 agent 而输入区有图时提示发送时不会带上
- **显示**：自己刚传的图直接用本地数据（同时写进 IndexedDB 的 `blobs` 缓存，键是 `blob_id`）；历史里的图先查缓存，没有再懒加载（`blob.get` 分段拉、同一节点同时最多 2 张），拉完写缓存，用 `blob:` URL 显示（CSP 已允许 `img-src blob:`）。点开是一个对话框看大图，带文件名、尺寸和下载
- 导出 Markdown 时附件写成 `[图片：<文件名>]`

#### 底部的分支 chip（ADR-34）

有 worktree 的会话在输入框下面选项行的最左边显示 `⑂ anycon/fix-login ← main  ↑3 ↓1  · 5 个未提交`（`data-testid="session-branch"`；没有的部分不显示）。数据来自 `SessionSummary.worktree` 和 `session.git_status`：进入会话页、`turn_done`、`file_changed`（防抖 2 秒）、`git` 事件、手动刷新时拉一次，不轮询。`state = creating` 显示转圈和「正在创建 worktree…」，`missing` 显示警告色「worktree 不见了」，`kept` / `removed`（已归档）显示中性色。点击切到主区的「改动」视图（见下）。「改动」视图没有输入框时它在底部单独一行；已归档的会话在「已归档」条的最前面；手机（< 768px）上它和其他选项一起在「⚙」底部弹层里。在项目目录本身工作的 git 会话只显示分支名（`git_status.branch`）和未提交数，不显示 base。

#### 信息面板

右栏只有**概览**（下面原有的内容）。**改动**不放在右栏里：开发者要看全部文件和 diff，需要一整块地方。

**改动**视图：会话在 git 仓库里时，顶栏右侧有「聊天 / 改动」开关（`main-view`，`main-view-chat` / `main-view-changes`，各带图标；有未提交文件时「改动」后面显示文件数 `main-view-changes-count`，即 `git_status.uncommitted`；手机上只显示图标），点分支 chip 也切过去。它是和聊天区一样大的主视图，占满顶栏以下的全部空间；消息流和输入框让出位置（草稿在 store 里，切回来还在；审批横幅仍显示，点「查看」会切回聊天）。默认停在聊天，选择只对当前会话有效。内容：

- 顶部一行状态：分支 ← base、ahead / behind、未提交、冲突；上游和未推送数；PR（有的话）：`PR #12 · 已打开 · ✓3 ✗1 ⏳2`，点开在新标签页打开 `url`（`rel="noopener noreferrer"`）。PR 只在进入这个视图和点「刷新」时查（`include_pr: true`），其余刷新不查
- 操作（按钮 + 「更多」菜单）：
  - **同步 base**（worktree 会话，`behind > 0` 时高亮）：`session.sync_base`；`uncommitted_changes` 时提示「先提交或丢弃未提交的改动」并提供「让 agent 提交」；`conflicts` 时显示冲突文件 + 「让 agent 解决」「放弃合并」（`abort: true`）；`merging` 状态下按钮换成这两个
  - **让 agent 提交**、**创建 PR**、**让 agent 解决冲突**：快捷消息，经 `session.send` 发给 agent（turn 在跑时进入排队，和普通消息一样）。文案按界面语言，带上分支和 base，例如「把当前的改动提交到分支 anycon/x，提交信息概括改了什么」「把分支 anycon/x 推送到远端，用 gh pr create 向 main 开一个 PR，写清楚标题和描述」（base 是提交而不是分支时——`base` 是 `base_sha` 的前 12 位——不写目标分支，让 agent 用仓库的默认分支）「合并 main 时出现了冲突，解决 `git status` 里标为冲突的文件并完成合并提交」（不把仓库里的文件名拼进用户消息）。推送和 `gh` 走 agent 的权限审批
  - **刷新**
- 分段控件「相对 base / 未提交」（在项目目录本身工作的会话只有「未提交」），下面是文件列表（`session.diff`）：路径、状态（新增 / 修改 / 删除 / 改名，`untracked` 标「未跟踪」）、`+N −M`，二进制标「二进制」；点一个文件展开它的 diff（`session.diff { path }` → 复用 `DiffView`），`patch_truncated` 时提示「只显示了前 256 KiB」。`truncated` 时列表底部提示
- 刷新时机同分支 chip；切换会话时清空。进入这个视图时先出 git 状态，再单独发一次 `include_pr` 请求出 PR
- 已归档的会话：快捷消息和「同步 base」禁用（先取消归档）；worktree 不是 `active` / `kept`（创建中 / 不见了 / 已删除 / 没建成）时只显示说明

**概览**标签：

- agent、模型、思考等级、权限模式（都可点开修改，规则见 § 5.3）
- 工作目录、创建时间、turn 数、累计 token 和费用
- 待审批请求列表（和消息流里的卡片联动）
- 「本会话改动的文件」：从 `file_changed` 事件汇总，点击在文件浏览（v0.2）打开，v0.1 只列路径
- 操作：重命名、归档、删除（二次确认，说明会删 daemon 上的历史）、导出 Markdown
- **有 worktree 的会话**：「归档」先弹确认框：说明 worktree 的处理（`auto`：没有未提交的改动就删掉 worktree——分支上有自己的提交就保留分支，没有就一起删；有未提交的改动就保留 worktree），选项「自动 / 保留 worktree / 删除 worktree（丢弃 N 个未提交的改动）」，N 来自 `git_status.uncommitted`。「删除」确认框多两项：worktree 同样三选一；「同时删除分支 anycon/x」复选框，分支上有自己的提交（`ahead > 0`）或没推送的提交时在旁边警告「N 个提交会丢失」。没有 worktree 的会话、worktree 已经 `removed` / `failed` 的会话行为不变（归档不弹框）

### 4.7 文件浏览 `/n/:node/files/*`（v0.2）

只读。目录树 + 文件预览（文本高亮、图片、Markdown 渲染），大文件只显示前 1 MiB 并提示。路径限制在 workspace 内，由 daemon 保证。

### 4.8 节点设置 `/n/:node/settings`

| 标签 | 内容 |
|---|---|
| **Agent** | `agent.list` 的结果：每个 agent 一张卡，状态（就绪 / 未安装 / 未登录 / 版本过低 / 异常）、版本、路径、问题提示和修复建议（「在那台机器上运行 `claude` 完成登录」）。「重新检测」按钮。每个就绪的 agent 下可设置**该节点上的默认选项**（模型、思考、权限模式），存在 daemon（`node.set_agent_defaults`） |
| **设备** | 和这台 daemon 配对的设备列表、在线状态、吊销。「添加设备」→ 让 daemon `pair_begin`（`node.pair_begin { long }`），页面上显示短码和二维码（链接是 `/pair#code=…`）。对话框里有「长码（16 位）」开关，默认开（`long: true`，二维码 / 链接带的就是 16 位码，经公共 hub 配对时用它）；关掉则重新请求一个 8 位短码（每次切换都会重新请求，新的顶掉旧的登记）。吊销是两步：hub 侧撤路由 + daemon 删公钥（`node.revoke_device`），任一步失败都显示错误，不吞。被吊销的浏览器下次连接看到「需要重新配对」，重新配对即可。「添加设备」对话框分两页：**短码**（上面的内容）和**接入码**（ADR-36：`node.enroll_code` 生成的 6 位码，大号显示 + 5 分钟倒计时 + 一句「新设备登录同一账号，选这台节点 →「我有接入码」」；关掉对话框不作废，过期后可重新生成，新的顶掉旧的）。另有「扫码批准新设备」按钮（§ 4.3.2） |
| **接入** | ADR-36。**节点口令**：状态（未设置 / 已启用 / 已停用 / 已锁定、连续失败次数、下次允许尝试的时间、设置时间和设置者，`node.enroll_status`）；「设置口令」/「修改口令」（两次输入、至少 8 个字符、`autocomplete="new-password"`，`node.set_enroll_password`）；「清除口令」（确认）；启用 / 停用开关（`node.set_enroll_enabled { password }`）。锁定时显示红色说明和「重新启用」，确认框写「连续 10 次输错可能是有人在猜。确认是你自己输错的再启用；不确定就先改口令」。**批准接入**开关（`node.set_enroll_enabled { approval }`，关掉后扫码批准和接入码都不可用）。**待批准的接入请求**（`node.enroll_list`，每条可批准 / 拒绝，和批准对话框同一套）。**最近的接入活动**（`node.audit` 里的 `enroll.*`，最近 20 条） |
| **Workspace** | 根目录路径、项目数、回收站内容（可恢复 / 清空）、scratch 目录大小。**Worktree** 一节（`workspace.worktrees`，ADR-34）：每行项目、分支、所属会话（链接；会话已删除时「没有会话」）、状态（使用中 / 已保留 / 已删除 / 不见了）、未提交数、自己的提交数、大小、创建时间；「删除」（`workspace.remove_worktree`）：会话还没归档的禁用并提示「先归档会话」；确认框里有未提交改动时要勾「丢弃未提交的改动」（`force`），另有「同时删除分支」（`delete_branch`，`ahead > 0` 时警告） |
| **限制** | 并发会话上限、空闲超时、是否允许 `auto` 权限模式。v0.1 只读显示并提示在机器上改 `config.toml`；v0.2 可远程改（`node.set_config`） |
| **关于** | daemon 版本、协议版本、运行时长、数据库大小、「检查更新」 |

### 4.9 账号设置 `/settings`

| 子路由 | 内容 |
|---|---|
| `/settings` | 显示名；**邮箱**（配了 SMTP 才显示这一块）：没绑定时「绑定邮箱」→ 输入邮箱 → 发码 → 输入 6 位码 → 生效；已绑定时显示地址，可「更换」（同一流程）和「解绑」（确认；是最后一种登录方式时按钮禁用并说明）。`409 email_taken` 显示「这个邮箱已经被别的账号绑定了」。绑定成功后提示「以后可以在任何设备上用这个邮箱登录」 |
| `/settings/login` | 登录方式：token 列表（新建只显示一次）、passkey 列表（添加 / 改名 / 删除）、已关联的 GitHub / Google（关联 / 解除）。最后一种不能删，按钮禁用并说明 |
| `/settings/sessions` | 浏览器登录会话，可踢出 |
| `/settings/devices` | 本账号全部设备（含当前浏览器，标「这台」），按节点分组显示配对关系，吊销 |
| `/settings/notifications` | **本浏览器的推送**：状态（未开启 / 已开启 / 被浏览器拒绝 / 不支持——不是安全上下文、没有 `PushManager`、iOS 没加到主屏）；「开启推送」→ `Notification.requestPermission()` → `pushManager.subscribe({ userVisibleOnly: true, applicationServerKey })` → `POST /api/push/subscriptions`（带本浏览器的 web 设备 id；还没配对过任何节点的浏览器没有设备 id，提示先配对或解锁一台节点）；「关闭」退订并 `DELETE`；「发送测试推送」→ `POST /api/push/test`。订阅过期 / 被换掉（`pushsubscriptionchange`）时 service worker 重新订阅并上报。**每个类别**一行 push / email 两个开关（`GET` / `PUT /api/me/notifications`；`email_available = false` 时邮件那一列禁用并说明原因：hub 没配 SMTP，或账号没绑定邮箱）。**本账号的其他推送订阅**列表（hub 给的 `device_name`、推送服务主机名），可删除。测试推送的结果按 `{ result, status? }` 显示（`error` 时带上推送服务回的状态码） |
| `/settings/appearance` | 主题（跟随系统 / 浅 / 深）、语言、字号、紧凑模式、思考过程显示（折叠 / 展开 / 隐藏）、Enter 行为 |
| `/settings/data` | 本地缓存大小、清空缓存、导出本浏览器的设备密钥（否——不提供，见 ADR-23）|
| `/settings/quota` | 仅 `multi`：配额用量 |
| `/settings/security` | 安全活动（hub 审计里本账号的部分） |

**退出**有两种，账号菜单里分开放：

- **退出登录**：结束 hub 的登录会话（cookie），清内存里的状态，并退订本浏览器的推送（浏览器的订阅和 hub 上的记录都删；锁屏上不该再出现这个账号的会话标题，下次登录要在通知设置里重新开启）。**IndexedDB 里的 Noise 私钥、device token 和明文会话缓存都保留**，下次登录同一个浏览器仍是同一台设备。退出前的确认框要说明这一点；退出请求失败要提示，不吞
- **退出并移除本设备**：确认框里确认后依次做（另外会退订本浏览器的推送订阅、删 `anycon-sw` 库）：对每个已配对节点 `node.revoke_device { device_id: 本设备 }`（节点说 `not_found` 当作已完成）→ `DELETE /api/devices/:id`（404 当作已完成）→ `POST /auth/logout` → 清本地数据（停传输，删整个 IndexedDB，清 localStorage / sessionStorage），最后整页跳到 `/login`。公用 / 借来的电脑用这个。**任何一步失败（含节点离线）都在清本地数据之前停下**，列出哪些节点没吊销成功，本地数据原样保留（清了就没法重试）；用户可以选「仍然清除本地数据」，这时离线的节点会继续持有这台浏览器的公钥，要等它们回来从设备列表里移除。**多标签页**：清库前先通过 `BroadcastChannel`（退回 `storage` 事件）通知同源的其他标签页，它们停掉传输和草稿定时器、禁用自己的 IndexedDB 句柄（之后的读写都抛错，不会把私钥写回去）、跳到 `/login`；另一个标签页删库触发的 `versionchange` 走同一条路

### 4.10 管理 `/admin`

`single` 模式管理员：邀请链接（生成、复制、撤销）、账号列表、hub 状态（§ hub.md 2.4）。`multi` 模式管理员多一个账号搜索和删除。

## 5. 模型、思考、权限模式

这是用户最常碰的三个选项，值得单独说。

### 5.1 数据从哪来

**Web UI 不内置任何 agent 知识。** 一切来自 daemon 的 `agent.list`（`AgentInfo`，结构见 [daemon.md § 5.2](daemon.md#52-agentinfo)），和 UI 相关的三块：

```
models: [ { id: "claude-opus-5-5", label: "Opus 5.5", description, default: true,
            thinking: { options: [ low | medium | high | xhigh | max ], default: "medium" } },
          { id: "claude-haiku-4-5", label: "Haiku 4.5", thinking: null } ]
allow_custom_model: true
modes:  [ { id: "default",           label: "Always Ask",        default: true, risk: "normal" },
          { id: "acceptEdits",       label: "Accept File Edits",                risk: "normal" },
          { id: "plan",              label: "Plan Mode",                        risk: "safe" },
          { id: "bypassPermissions", label: "Bypass",                           risk: "dangerous" } ]
options: [ { id: "fast_mode", kind: "bool", label: "Fast mode", default: false } ]
```

要点：**思考等级挂在模型上**（不同模型支持的等级不同，有的不能关）；**权限模式是 agent 自己的集合**（Claude Code 六种、Codex 四种、ACP agent 自定义），UI 只认 `risk`；其他选项走通用 `ConfigOption`，折叠进「更多选项」。新 agent 多一个模式或选项不需要改 Web UI。

### 5.2 控件

| 选项 | 控件 | 细节 |
|---|---|---|
| 模型 | 下拉 | 显示 `label`，副标题 `description`，默认项有标记，`tags` 里的 `1m` 之类显示为小徽章。`allow_custom_model` 时底部有「自定义…」可输入任意 id，输入过的自定义值记在本地供下次选择。agent 只有一个模型时不显示 |
| 思考 | 下拉 | 显示**当前所选模型**的 `thinking.options`，切换模型时如果原等级不在新模型的列表里，回落到新模型的默认值并提示。等级名不翻译（low / high 是 agent 的词汇）。所选模型 `thinking` 为空时不显示 |
| 权限 | 下拉 | 显示 `modes[]` 的 `label` 和 `description`。`risk = dangerous` 的项带警示图标，选中时弹确认框说明风险，确认后头部状态 chip 变红并一直显示该模式名；节点禁止危险模式时（`allow_dangerous_modes = false`）daemon 已把这些项从 `agent.list` 的 `modes` 里去掉，所以通常根本不出现；若 `agent.list` 缓存过期、`session.set_settings` 仍被拒（`forbidden` / `invalid_settings { field: "mode" }`），该节点的危险项禁用并说明。`risk = safe`（如 plan）用绿色标识 |

### 5.3 什么时候能改

- **新建会话时**：composer 选项栏全部可改
- **会话空闲时**：全部可改（`session.set_settings`），从下一个 turn 生效，消息流出现「模型已切换为 …」。adapter 内部是热改还是重启进程续接，UI 不关心（[daemon.md § 6.4](daemon.md#64-会话中修改)）
- **运行中的 turn**：所有选项锁定，等本轮结束；提供「结束后应用」的排队改动

### 5.4 默认值

优先级从高到低：

1. 用户在这个浏览器上，对这个 `(节点, agent)` 上次用的值（localStorage）
2. 节点设置里该 agent 的默认选项（存在 daemon，所有客户端共享）
3. agent 自己的默认（`choices[].default`）

例外：`risk = dangerous` 的权限模式**不会**被自动带进新会话——不管它来自「上次用的值」、节点默认值还是 `agent.list` 里的 `default` 标记，Web 在自动选择默认值时一律跳过危险模式（回落到下一级里的非危险项），危险模式每次都要显式选并确认。节点默认值（第 2 级）daemon 会反映到 `agent.list` 的 `default` 标记里，所有客户端都读得到；daemon 自己也不接受把危险模式写成默认值，除非 `allow_dangerous_modes` 允许（[daemon.md § 5.6](daemon.md#56-rpc)）。

### 5.5 思考过程的显示

`thinking_delta` 是否出现取决于 agent 是否输出（capabilities.`thinking_stream`）。显示策略在外观设置里：折叠（默认）/ 始终展开 / 隐藏。隐藏时仍然缓存，只是不渲染。

### 5.6 用量与费用

`turn_done.usage` 有什么就显示什么：token 数、费用、用时。agent 不报的字段不显示占位。会话累计显示在信息面板，节点首页不汇总（daemon 不做记账，这是 agent 的事）。

## 6. 状态层与缓存

### 6.1 store 划分（zustand）

| store | 内容 | 持久化 |
|---|---|---|
| `auth` | 账号信息、登录方式 | 否（每次从 `/api/me` 拉） |
| `hub` | WebSocket 状态、本设备 id、配额 | 否 |
| `nodes` | 每个节点：Noise 会话状态、在线状态、`node.info`、`agent.list` 缓存 | `node.info` 和 `agent.list` 进 IndexedDB |
| `sessions` | 会话列表、每个会话的事件（按 seq） | IndexedDB |
| `drafts` | 每个会话的未发送消息和草稿 | IndexedDB |
| `prefs` | 外观、Enter 行为、各 `(节点, agent)` 的上次选项、右栏开合 | localStorage |

hub 的 REST 接口用 TanStack Query，不进 zustand。

### 6.2 IndexedDB

```
keys        { device_id, static_keypair }                 本设备 Noise 密钥
nodes       { node_id, name, info, agents, last_seen }
sessions    { session_id, node_id, project, agent, title, status, options, last_seq, updated_at }
events      { [session_id, seq], event }                 主键是复合键，范围查询拉增量
drafts      { session_id, text, attachments?: [AttachmentRef], unsent: [{ …, attachments? }] }
blobs       { blob_id, bytes, mime, last_used_ms }       图片缓存（ADR-35），LRU 上限 200 MiB
```

按 origin 隔离，这是 [ADR-23](../decisions.md#adr-23-token-登录与局域网双-origin) 接受的代价。

`keys`（Noise 私钥、device token）和 `events`（会话明文缓存）是**明文**存的，在浏览器的存储保护之外没有额外加密。这些代码来自 hub（[architecture.md § 5.6](../architecture.md#56-威胁模型)），能读到它们；「退出登录」不清它们，「退出并移除本设备」才清（§ 4.9）。

### 6.3 同步算法

每次和某个节点建立 Noise 会话后：

1. `node.info` → 校验协议版本，更新能力。版本不兼容就停在这里
2. 其余几步**同时发出**，不互相等待：每个请求都要经过一次 浏览器 → hub → daemon 的往返，串行发会多花好几个往返时间；daemon 对同一客户端仍按到达顺序处理（protocol.md § 7.3）。某一步出错只影响它自己那块数据，链路断了整轮作废、重连后再来
   - `agent.list { if_none_match }` 刷新 agent 缓存；`workspace.list`（有 `workspace` 能力时）；`permission.list` 拉未决请求；`node.devices`；`node.enroll_list`
   - `session.list` → 和本地比对：新会话插入；`last_seq` 更大的标记需要增量；本地有而远端没有的标记已删除。比对完之后处理当前打开的会话（下一步要用比对结果判断缓存是否作废）
3. 当前打开的会话：`session.watch` 和 `session.history(after_seq)` 一起发（按到达顺序处理，watch 先生效，不会漏事件）；其他会话在打开时再拉。离开页面 `unwatch`。列表级变化（标题、状态、待审批数）通过 `session.changed` 推给所有在线客户端，不需要 watch
4. 全部完成后重发未发送的消息

连接各阶段（加载、连 hub、`auth_ok`、握手、就绪、每个同步请求、打开的会话就绪）用 `performance.mark` / `measure` 记录，名字带 `anycon:` 前缀，DevTools 的 Performance 面板可见；`localStorage['anycon.debug.timing'] = '1'` 时在控制台打印耗时表，控制台里 `anyconTiming()` 随时打印全部记录。只记阶段名、方法名和 id 前缀。

### 6.4 连接状态机（每个节点）

```
hub_disconnected → hub_connecting → hub_authed
    → peer_offline              （节点不在线，可看缓存、可写草稿）
    → noise_handshaking → ready （正常）
    → noise_failed              （daemon 不认识我们的公钥：被吊销或 daemon 重装，提示重新配对）
```

hub 重连用指数退避 1s → 30s 加抖动；收到 `4002 restarting` 用 1–3 秒。页面从后台回到前台（`visibilitychange`）立即触发一次重连检查，这对手机很重要。

**配对完成的最后一步：`pair.ack`**。配对不是在 hub 发 `pair_done` 时结束的。握手成功、收到 `pair_done` 之后，浏览器用刚建立的 Noise 传输等 daemon 发来的第一条通知 `pair.ack`（`{ node_id, peer_id, name, version, fingerprint }`），逐项校验：`peer_id` 等于 hub 在 `pair_done` 里告诉自己的正式设备 id；`node_id` 等于 `pair_done.peer`；`fingerprint` 等于本地对握手得到的 daemon 静态公钥算出的指纹。全部通过才把节点（id、**认证过的**名称、公钥）写进 IndexedDB 并显示为已配对；节点名取 `pair.ack` 的，不取握手第 2 条里未经认证的那个。30 秒没收到、或任何一项对不上，就放弃，什么都不保存。本地已有同 id 但公钥不同的节点时不覆盖，提示用户先忘记旧节点。这样一个伪造 `pair_done` 的 hub 骗不了浏览器（它没有 PSK，造不出 `pair.ack`）。

**传输的分片与重置**。每个节点一个 Noise 传输和一个分片重组器。发：一条 RPC 的密文超过 200 KiB 就拆成多个 `data` 信封（每条 RPC 一个本地递增的 `msg_id`）。发出的 `data` 信封原文保留在一个 16 MiB 的**在途窗口**里：10 秒内发出的和等待重发的永不逐出，窗口满了新信封在出站队列里按序等（握手不受窗口限制），不逐出。收到 `backpressure { to, seq }` 只暂停**那个节点**（新信封排队，握手和其他节点照发），等 `retry_after_ms`（最近一条 `rate_limited` 给的，封顶 30 s；没有就 100–300 ms 加抖动）后把所有被退回的 `seq` 按升序原样重发（不重新加密），再放行排队的；`rate_limited` 暂停整条连接。被退回的 `seq` 已不在窗口里（10 秒后才到的退回）、或同一节点连续 60 秒都被退回 → 重置传输重新 IK。传输被丢弃时，这个节点排队中和被退回的信封一起丢掉并立刻释放原文，只留 10 秒的「seq → 节点」墓碑，迟到的退回不会重置新传输。**握手被退回**（hub 限速或缓冲满）不重置也不立即重发：等 300–1000 ms（逐轮翻倍、封顶 30 s，且不早于 `rate_limited` 过期）后原样重发同一条握手消息，等待期间握手超时不计时，所以长时间限速不会被误判成 `noise_failed`。收：分片按 `msg_id / index / count` 严格按序重组后再解密。**解密失败、分片乱序或收到 RESET（`0x02`）都意味着传输已失步**：丢掉这个节点的传输、清空重组器，直接重新发起 IK 握手（状态回到 `noise_handshaking`），不是整页报错。收到 `dropped { too_large }` 说明自己没分片，是 bug，记 console error。握手重试仍失败才进入 `noise_failed`。

### 6.5 加密在哪跑

Noise 的 wasm 在主线程跑：单条消息加解密是微秒级，开 Worker 得不偿失。例外是 Argon2id（短码配对和节点口令解锁各做一次，§ 4.3 / § 4.3.1；约 0.1–1 s、64 MiB wasm 内存）：目前也在主线程，调用前先让 UI 画出「正在派生密钥…」再算，手机上的实际耗时还没有实测；移到 Worker 是已知的改进项。接入用的 CPace 只是几次标量乘法，毫秒级，不用单独处理。附件分块（512 KiB 一块）的 base64 和加密也在主线程，块与块之间让出主线程，不卡输入。

## 7. 通知

- **Web Push**（ADR-37）：hub 只在这台浏览器**没有在线连接**时推（[hub.md § 6.1](hub.md#61-web-push)）。service worker 收到 `{ category, node, session }` → 从 IndexedDB 读本地缓存的节点名和会话标题组装文案（没有缓存就用「一个会话」/「一台节点」，文案逻辑和页面共用 `features/sync/pushText.ts`）→ `showNotification`（每条推送都要显示，`userVisibleOnly`）。通知的 `tag`：会话类是 `session:<会话 id>`（同一会话的各类别合并），第二行是「来自 <节点名>」。界面语言取页面记在独立 IndexedDB 库 `anycon-sw`（`kv.lang`）里的值，没有才跟 `navigator.language`；service worker 只读页面的主库、从不创建它。点击：有已打开的本 origin 窗口就聚焦，用 `postMessage({ type: 'anycon:navigate', url })` 让页面站内跳转，没有就 `openWindow`；会话类打开 `/n/:node/s/:session`，接入类打开 `/n/:node/settings?tab=enroll`（`enroll_requested` 带 `review=1`）。同一会话 / 同一节点的接入类用 `tag` 合并。`category = test` 显示「Anycon 测试推送」
- **页面在后台**：页面转到后台（`visibilitychange` → hidden）满 30 秒就主动断开和 hub 的连接，回到前台再连（§ 6.4 的重连）。这样 hub 能看出这台浏览器「不在」，改为推送——手机上被冻结 / 挂起的页面收不到信封，但浏览器的网络层会替它回 WebSocket 的 ping，不主动断开的话 hub 会一直以为它在线、一条推送也不发。断开之前那 30 秒里收到 `notify` 信封、本浏览器**开着推送**（§ 4.9）且已授权通知时，页面经 service worker 的 `registration.showNotification` 弹同样的系统通知（同样的 `tag`），并且遵守账号偏好里这个类别的推送开关（读 `/api/me/notifications`，缓存 60 秒，读不到当作开）；本浏览器关了推送就不弹。已知限制：页面一转到后台就被系统直接冻结的（iOS 主屏 PWA 几秒内就会被挂起）计时器跑不到 30 秒，hub 会继续把它当作在线、不改发推送，直到系统真正关掉这条连接（`freeze` / `pagehide` 还没有接）；正在配对 / 接入或还有请求没回来时不断开，每 5 秒再试
- **应用内**：页面在前台时不弹系统通知，改为右上角 toast + 左栏会话条目的小红点；待审批的会话在左栏置顶
- **角标**：`navigator.setAppBadge(待审批总数)`，支持的平台上 PWA 图标显示数字
- **接入相关**（ADR-36）：`enroll_requested` 和 `security` 没有 `session`，文案是「有设备请求接入 <节点名>」/「<节点名> 的安全提醒」，点击打开该节点设置的「接入」标签（`enroll_requested` 时自动弹出待批准请求的对话框）；同一节点的用 `tag` 合并。页面在前台时分别是 § 3.4 的横幅和 toast
- **声音**：默认关，设置里可开，只对 `permission_requested` 和 `enroll_requested`

## 8. PWA

- `vite-plugin-pwa` 用 `injectManifest`：service worker 是自己写的 `src/sw.ts`（只做 workbox 接线，逻辑在 `src/sw/` 的纯函数里，可单测）（workbox 预缓存 + `push` / `notificationclick` / `pushsubscriptionchange`），`display: standalone`，主题色跟随明暗
- 首次访问后静态资源全部缓存，离线能打开壳子并看缓存的会话
- 更新策略：新 service worker 就绪后不自动刷新，显示横幅（§ 3.4 第 6 条），避免在用户正看流式输出时刷掉
- iOS：`apple-mobile-web-app-capable`、启动图、`viewport-fit=cover`

## 9. 快捷键

| 键 | 作用 |
|---|---|
| `⌘K` / `Ctrl+K` | 命令面板：搜会话、项目、节点，执行「新建会话」「切换节点」。v0.1 只搜本地已知的标题，不是 daemon 的 FTS |
| `⌘N` | 当前项目新建会话 |
| `Enter` / `Shift+Enter` | 发送 / 换行（可对调） |
| `Esc` | 运行中：聚焦「停止」按钮（再按一次确认）；否则关闭弹层 |
| `⌘⇧A` / `⌘⇧D` | 允许 / 拒绝最近一条待审批 |
| `⌘.` | 开合右栏 |
| `⌘B` | 开合左栏（宽屏） |
| `[` / `]` | 上 / 下一个会话 |

按住不放产生的重复按键（`e.repeat`）一律忽略；在输入框里打字时 `⌘⇧A` / `⌘⇧D` 不生效，而且它们只处理 `tool` 类请求、只选一次性选项（`allow_once` / `reject_once`），不会悄悄升级成「总是允许」。

## 10. 国际化、主题、可访问性

- 文案全部走 i18n，v0.1 出 **英文和中文**两份；公共 hub 面向全球，英文是默认语言，按浏览器语言自动选
- 主题 token 在 `web/DESIGN.md`：浅 / 深两套，跟随系统；代码高亮用 shiki 的双主题输出
- 所有交互组件基于 Radix Primitives，保证焦点管理和键盘可达；流式输出区域 `aria-live="polite"`，但 token 级增量不播报，只在 turn 结束时播报一句
- 字号三档，紧凑模式减小气泡间距

## 11. 空状态与错误

| 场景 | 展示 |
|---|---|
| 这个浏览器没有节点 | 账号下有节点：`/unlock`；一台都没有：`/setup` 向导 |
| 节点没有项目 | 插图 + 「新建项目」「克隆仓库」「打开已有目录」三个按钮 + 一句「或者直接发一条消息开始临时会话」 |
| 项目没有会话 | 直接聚焦 composer |
| 节点没有可用 agent | 卡片列出检测到的问题和修复命令，composer 禁用 |
| 节点离线 | 黄条 + 缓存内容灰化；composer 可用，发送即「未发送」 |
| Noise 握手失败 | 整页提示「这台节点不再认识此设备」+「重新解锁」（`/unlock/:node`）+「用短码配对」 |
| 会话被其他设备删除 | 当前页提示并返回项目页 |
| 协议版本不兼容 | 整页提示升级 daemon 或刷新 Web UI，给出两边版本号 |

## 12. 版本范围

**v0.1**：登录（token；passkey 推迟到 v0.1.x）、`/setup`、`/pair`、节点首页、项目页、会话页（含三个一等选项、权限卡片、思考折叠块）、节点设置（Agent 只读、设备）、账号设置（登录方式、设备、通知、外观）、IndexedDB 缓存、PWA、英文 + 中文。`/admin` 只有邀请和 hub 状态。会话选项里的 `system_prompt_append`、`max_turns`、`max_budget_usd`、`env` 在 Web 和 CLI 里都没有输入框，目前只能经 SDK 设置。

v0.1 另含 ADR-34：新建会话的工作位置（worktree）、分支 chip、「改动」标签（只读 diff、同步 base、快捷消息、PR 状态）、归档 / 删除时的 worktree 处理、节点设置里的 worktree 列表。

v0.1 另含 ADR-35：图片附件（上传、显示、缓存）。

v0.1 另含 [ADR-37](../decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)：登录页的邮箱验证码、账号设置里的邮箱绑定、通知设置页（本浏览器推送的开关和测试、每个类别的推送 / 邮件开关）、service worker 的推送和点击处理、页面在后台时的系统通知。

v0.1 另含 [ADR-36](../decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)：`/unlock`（三种解锁方式）、批准对话框、应用内扫码、「添加设备」的接入码、节点设置的「接入」标签、`/setup` 里的口令提示、接入相关的推送。

**v0.2**：`multi` 模式的邮箱注册和 OAuth 登录、非图片附件、文件浏览、搜索、节点设置远程修改、`/admin` 完整版、配额页。（导出会话为 Markdown 在 v0.1 已经有，客户端本地生成。）

**v0.3**：语音输入、命令面板的更多动作、模板创建项目、多客户端同时在线的光标 / 正在输入提示。
