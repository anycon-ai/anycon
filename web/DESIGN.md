# Web UI 设计规范

一页纸：token、间距、字号、暗色、组件约定。真源是 `src/index.css`（CSS 变量 + Tailwind 4 `@theme`），改这里的数值就改那个文件。

## 原则

- **手机上能用，桌面上好用。** 手机单栏栈式导航、底部固定输入区、选项收进底部弹层；≥ 768px 左栏常驻；≥ 1200px 三栏。
- 视觉克制：中性灰 + 一个强调色（靛蓝）。状态色只表达状态（成功 / 警告 / 危险 / 信息），不做装饰。
- 组件自己写，行为层用 Radix Primitives（焦点管理、键盘可达、ARIA）。图标用 lucide。
- 所有文案走 i18n（`src/i18n/en.json`、`zh.json`，测试保证两份键一致）。

## 品牌与图标

- **名字**：字标写作 `Anycon`（界面上的 `Logo`、页面标题、PWA 名称、iOS 主屏名）。命令、包名、代码里的产品名仍是小写 `anycon`。
- **主标**：一个 A，中间挖出一个 C——A 连着 C，就是 anywhere connect。设计稿原件就放在 `public/`，不由代码生成：

  | 文件 | 用途 |
  |---|---|
  | `logo.svg` | 彩色主标（蓝紫渐变 + 折面高光），界面里的 `Logo` 组件内联的就是它 |
  | `favicon.svg` | 浏览器标签页：单色 `#305CFF`，viewBox 裁到 12–244 让小尺寸下更饱满 |
  | `logo-mono-blue.svg` / `logo-mono-black.svg` / `logo-mono-white.svg` | 单色版（`#305CFF` / `#141B2D` / `#FFFFFF`），给只能用单色的场合：深色底用白，打印 / 浅底用黑 |
  | `icon-192.png` / `icon-512.png` | PWA（`purpose: any`），透明底的彩色主标 |
  | `icon-maskable-512.png` | PWA（`purpose: maskable`）：白底铺满，主标缩到 0.68 以落在直径 80% 的安全圆内 |
  | `apple-touch-icon.png` | iOS 主屏：180px、白底铺满、不带圆角（系统自己套），主标缩到 0.8 |

- 四张 PNG 由 `node web/scripts/icons.mjs` 从 `logo.svg` 渲染（用 e2e 里装好的 Playwright Chromium，根目录 `bun install` 之后即可）。不要手改 PNG。
- 换设计稿：替换 `public/` 里的 SVG → 把外形路径同步到 `src/components/Logo.tsx`（`Logo.test.tsx` 比对三处路径是否一致）→ 重跑脚本出 PNG。
- 所有 SVG 的外形是同一条 evenodd 路径；彩色版的三层渐变都裁在这条路径里面。

## 颜色 token

用 CSS 变量定义，`@theme inline` 映射成 Tailwind 工具类（`bg-bg`、`text-fg-muted`、`border-border`、`bg-accent` …）。`<html class="dark">` 切换暗色；偏好为「跟随系统」时由 `ThemeEffects` 监听 `prefers-color-scheme`。`index.html` 在首帧前加载 `/theme.js` 套用主题，避免闪白（CSP 不允许内联脚本）。

| token | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `bg` | `#ffffff` | `#121314` | 页面底色 |
| `bg-subtle` | `#f7f7f9` | `#191a1b` | 侧栏、卡片底、代码块底 |
| `bg-muted` | `#efeff3` | `#242526` | 行内代码、骨架、禁用底 |
| `bg-hover` | `#f1f1f5` | `#2b2c2d` | 悬停 |
| `surface` | `#ffffff` | `#202122` | 卡片、弹层 |
| `border` / `border-strong` | `#e4e4ea` / `#cfcfd9` | `#2a2b2c` / `#333536` | 分隔线 / 控件边框 |
| `fg` / `fg-muted` / `fg-subtle` | `#17171c` / `#565662` / `#858592` | `#bfbfbf` / `#8c8c8c` / `#777879` | 正文 / 次要 / 辅助（≥ 4.5:1 对比度的只有前两档，`fg-subtle` 只用于非关键信息） |
| `accent` / `accent-hover` / `accent-fg` / `accent-subtle` | `#4f46e5` / `#4338ca` / `#fff` / `#eef0ff` | `#297aa0` / `#2b7da3` / `#fff` / `#1f323b` | 主按钮、选中态底、边框 / 焦点环 |
| `accent-text` | `#4f46e5` | `#48a0c7` | 强调色的文字和图标（链接、选中勾、进行中图标）。深色下 `accent` 做文字对比度不够，所以单独一档 |
| `success` / `warning` / `danger` / `info`（各带 `-subtle` 底色） | `#15803d` `#a16207` `#dc2626` `#1d4ed8` | `#73c991` `#e5ba7d` `#f48771` `#57a3f8` | 状态；`danger` 同时是危险权限模式的标识色 |

规则：

- 不在组件里写十六进制色值，只用 token。需要半透明时用 `color-mix` 或 Tailwind 的 `/` 透明度写法（`border-danger/50`）。
- 权限 `risk`：`safe` → success 色；`normal` → 中性；`dangerous` → danger 色，且会话头部一直显示红色 chip。
- 代码高亮用 shiki 的双主题输出（`github-light` / `dark-plus`），颜色由 `--shiki-light` / `--shiki-dark` 变量切换；diff 的增删底色由 `success` / `danger` 混出。

## 间距

4px 基准（Tailwind 默认 `1 = 0.25rem`）。常用档位：

| 档 | 值 | 用在 |
|---|---|---|
| 1 / 1.5 | 4 / 6px | 图标与文字间距、chip 内边距 |
| 2 / 3 | 8 / 12px | 控件内边距、卡片内小间距、手机页面左右边距 |
| 4 | 16px | 卡片内边距、页面左右边距（桌面 16–24px） |
| 6 / 8 | 24 / 32px | 区块之间 |

- 消息之间的间距是变量 `--msg-gap`（默认 `1.25rem`，紧凑模式 `0.625rem`）。
- 圆角：`sm 6px`（小控件）、`md 10px`（按钮、输入框）、`lg 14px`（卡片、弹层）；用户消息气泡 `rounded-2xl`、`bg-muted` 浅灰底、正文色字；输入框是 `rounded-2xl` 的圆角框。
- 点击目标 ≥ 36px 高（手机输入区按钮 40px）。
- 安全区：`pb-safe` / `pt-safe`（`env(safe-area-inset-*)`），整页高度用 `100dvh`，键盘弹起时输入区跟随。

## 字号：三档

整个界面用 `rem`，改根字号就整体缩放。外观设置里的「文字大小」写到 `<html data-font>`：

| 档 | 根字号 | 说明 |
|---|---|---|
| 小 `sm` | 14px | 信息密度高 |
| 中 `md`（默认） | 16px | 正文 `0.9375rem` ≈ 15px |
| 大 `lg` | 18px | 手机上阅读更舒服 |

固定的层级（相对根字号）：页面标题 `1.25rem / 600`，区块标题 `0.75rem / 600 / 大写 / 字距`，正文 `0.9375rem`，控件 `0.8125–0.875rem`，辅助信息 `0.75rem`。代码一律 `--font-mono`，`0.8125rem`。中文回退到 PingFang SC / Noto Sans CJK / 微软雅黑。

## 暗色

- 配色取自 VS Code「2026 Dark」（`extensions/theme-defaults/themes/2026-dark.json`）：编辑器底 `#121314` → 侧栏 `#191a1b` → 弹层 / 卡片 `#202122`，边框 `#2a2b2c`；层次靠明度差而不是阴影。
- 强调色是 VS Code 的按钮蓝（`#297aa0`，白字）；作文字 / 图标用 `accent-text`（链接色 `#48a0c7`）。代码高亮用 shiki 的 `dark-plus`（VS Code 的 token 配色）。
- `fg-subtle` 没有直接用主题的 `disabledForeground`（`#555555`，对比度不到 3:1），提亮到 `#777879`。
- 阴影在深色下加深并拉大，只用于浮层（菜单、对话框、toast）。
- 图片 / 二维码这类自带底色的内容保持原样（二维码固定白底黑块，保证可扫）。

## 组件约定

- **按钮** `Button`：`primary`（每个区域至多一个）/ `secondary` / `outline` / `ghost` / `danger` / `outline-danger` / `outline-success`；`sm` 32px、`md` 36px、`lg` 44px、`icon` 36px。加载态自动禁用并显示转圈。
- **选项控件**：模型 = 下拉（显示 `label`、`description`、默认标记、`tags` 徽章）；思考 = 下拉（只列所选模型的等级）；权限 = 下拉（`risk` 决定图标和颜色，`dangerous` 选中前弹确认框）；其余 `ConfigOption` 折叠进「更多」。输入框下面那一行用无边框的文字按钮（`optionTrigger(false)`），左组项目 / 工作位置 / 权限 / 更多，右组 agent / 模型 / 思考；底部弹层里是撑满一行的带边框按钮。
- **输入框** `Composer`：圆角框里上面是缩略图（点开看大图），下面一行「＋」菜单、输入框（无边框，焦点环画在整个框上）、停止 / 发送（圆形）。「＋」是 Radix DropdownMenu，新入口加成菜单项。
- **目录选择器** `FolderPicker`（新建项目 →「已有目录」）：路径框 +「前往」、上一级按钮、面包屑（不能走出 root，多个 root 时多一个切换下拉）、当前目录一行（可选时有「选择当前目录」）、「显示隐藏目录」开关、子目录列表（`max-h-64` 自己滚动）。每行 ≥ 44px：点目录名进入，右侧「选择」才是选中；已是项目的行灰掉并标项目名，`linkable = false` 的标「不能关联」且没有「选择」。它常被放在 `<form>` 里，自己不用 `<form>`（回车在路径框里单独处理）。
- **分段控件** `Segmented`：`fill` 让它撑满一行、各项等宽、窄屏文字换行（新建项目的三种方式用它，手机上不横向滚动）。
- **状态 chip** `Chip`：圆角胶囊，底色用 `*-subtle`、文字用对应状态色。会话状态：运行中 accent、等待审批 warning、出错 danger、其余中性。
- **分支 chip** `BranchChip`（会话页底部：输入框上方靠右，ADR-34）：`⑂ 分支 ← base ↑N ↓N · N 个未提交`，胶囊保持 chip 大小、外面的按钮 ≥ 36px；`creating` 转圈 + 文字、`missing` / `failed` 警告图标 + 文字、`kept` / `removed` 中性并带状态词，状态不只靠颜色。手机（< 768px）在「⚙」底部弹层里。
- **单选 / 复选** `RadioChoices` / `CheckboxField`（`ui/Choice.tsx`，Radix RadioGroup / Checkbox）：每项是一整行可点的标签（≥ 36px），可带一行小字说明；归档 / 删除 worktree 的确认框和节点设置的 worktree 删除框用它。
- **卡片**：1px `border` + `surface` 底；需要强调时换 `border-warning/50`（待审批）或 `border-danger/50`。
- **覆盖层**：居中对话框（`Dialog`）、侧边抽屉（`Drawer`，手机上的左栏、窄屏的右栏）、底部弹层（`Sheet`，手机上的选项面板）。都基于 Radix Dialog，焦点困住、Esc 关闭。
- **无障碍**：所有可交互元素有可访问名称；状态不只靠颜色（带文字或图标）；流式输出区域不做 token 级播报，turn 结束时通过一个 `aria-live="polite"` 的隐藏区域播报一句；`prefers-reduced-motion` 下关闭脉冲 / 光标 / 转圈动画。
- **动效**：只有三种——状态点的缓慢脉冲、流式文本末尾的光标、加载转圈。不做页面转场。

## 响应式断点

| 宽度 | 布局 |
|---|---|
| < 768px | 单栏栈式；左栏是抽屉（顶栏菜单按钮）；会话页右栏是抽屉（顶栏「ⓘ」）；选项收进输入框左侧的「⚙」底部弹层 |
| 768–1199px | 左栏（260px，可收起，`⌘B`，状态记在 localStorage）+ 主区；右栏变成右侧抽屉 |
| ≥ 1200px | 左栏 + 主区 + 右栏（320px，可收起，状态记在 localStorage） |
