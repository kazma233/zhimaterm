# ADR-057：窗口顶栏——自绘标题栏统一承载全局入口

状态：已采纳（2026-11-16）

## 背景

全局入口此前分散三处：侧栏 header 行（⚙ 设置、? 快捷键，ADR-041 §2）、侧栏
footer 动作行（本地/WSL/SSH + 主题色板，ADR-041 §1）、窗口标准系统标题栏（只
显示跟随会话的标题文本）。Atr-041 语境里的「顶栏」指侧栏 header 行，不是窗口
标题栏——本次引入真正的**窗口顶栏**（类似 Zed 的 TitleBar）：应用自绘一条
~34px 的标题栏，系统标题栏退场（client-side decoration），全局入口全部收编
到这一条里。侧栏随之变成纯会话列表。

## 决策

### 1. 顶栏内容与布局

```
┌──────────────────────────────────────────────────────────────┐
│ ●●●   [本地] [SSH] ([WSL])    ◆ 活动会话标题    [色板] [?] [⚙] │ ← macOS
│        [本地] [SSH]      ◆ 活动会话标题    [色板] [?] [⚙] ─ □ ✕ │ ← Windows/Linux 自绘控制按钮
└──────────────────────────────────────────────────────────────┘
```

- **左区**：新建会话入口（本地 / WSL / SSH），取代侧栏 footer 动作行；顺序与
  ADR-041 相同（本地、WSL、SSH），WSL 仍仅 Windows 且常显（ADR-056 口径）。
- **中区**：活动会话标题（用户起的名字优先，否则自动标题，与侧栏行同一字符串，
  ADR-047/053 口径）；没有标签时不显示。窗口标题（`set_window_title`）继续跟随
  聚焦格标题，双保险不变——顶栏标题是「标签名」，窗口标题是「聚焦格标题」，
  分屏下二者可以不同。
- **右区**：主题色板、? 快捷键、⚙ 设置，取代侧栏 header 行的全局入口。
- **控制按钮**：Windows/Linux 在右端自绘最小化/最大化（还原）/关闭；macOS 用
  系统红绿灯（`traffic_light_position` 挪进顶栏高度内），左侧留 78px
  （TRAFFIC_LIGHT_PADDING，SDK 26+ 口径，红绿灯不随 rem 缩放）。
- 高度固定 34px 定值像素，与 STATUS_BAR_HEIGHT 同口径（红绿灯与窗口控制按钮
  都不随界面字号缩放，跟随缩放只会让三平台高度失配）。

### 2. 窗口选项（三平台统一代码，平台差异收在常量与按钮渲染）

- macOS：`appears_transparent: true` + `app_owns_titlebar_drag: true`。
  **必须**用后者：gpui 的 contentView 对 AppKit 声明整个视图为 app 拥有的
  titlebar 内容，AppKit 不再从标题栏区域拖窗、也不延迟派发点击（macOS 27 的
  双击判定延迟问题）——顶栏里的按钮才能收到点击。拖拽由应用自己驱动。
- Windows/Linux：`appears_transparent: true`（Linux 由
  `window_decorations: Some(Client)` 请求 CSD，compositor 可协商回 Server，
  运行时以 `window_decorations()` 实际值为准）。
- 几何持久化（ADR-037）：`appears_transparent` 后 macOS 走 FullSizeContentView，
  内容区矩形 == 窗口外框矩形，`window_bounds()` 保存/恢复两侧天然对称。
  **`main.rs` 的 `restore_window_bounds` 补偿与 `titlebar_height()` 整体移除**
  （那是标准标题栏时代的内容区/外框换算，见 main.rs 历史注释）。

### 3. 拖拽与双击（对齐 Zed platform_title_bar 的状态机）

- 顶栏根元素声明 `window_control_area(Drag)`；左键按下置 armed、移动即
  `start_window_move()` 并解除、抬起/按下在外部则解除。三平台同一段代码：
  Windows 上 HTCAPTION 原生命中短路了鼠标事件，状态机实际只在 macOS/Linux
  生效；macOS 走 `performWindowDragWithEvent`，Linux 走 xdg_toplevel move。
- 双击：macOS `titlebar_double_click()`（读系统「双击窗口标题栏」偏好）；
  Linux `zoom_window()`（最大化受支持且窗口可缩放时）；Windows 由 HTCAPTION
  原生命中处理，不走到这里。
- Linux CSD 下顶栏右键 `show_window_menu`（compositor 支持时）。
- 顶栏内的每个按钮在按下与点击时 `stop_propagation`，不武装拖拽、不触发根
  元素的双击判定。
- macOS 全屏时红绿灯由系统接管：顶栏去掉左侧 78px 留位、隐藏窗口控制按钮
  （与 Zed 同口径）。

### 4. 侧栏瘦身为纯会话列表

- header 行（「会话」标题 + ⚙ + ?）与 footer 动作行整体移除；侧栏只剩工作区
  错误行（如有）、会话列表滚动区、右缘拖宽手柄。会话行的行内 ✕、右键重命名
  菜单、拖拽排序不变。
- 主题下拉改为**向下弹出**（入口从侧栏底缘移到顶栏，向上展开会溢出窗口）；
  「点击一项立即切换且不收起」的交互契约（ADR-022）不变。

### 5. 模块归属

- 顶栏渲染收在 `app/src/views/title_bar.rs`：拖拽状态机、平台留位、窗口控制
  按钮与双击/窗口菜单是它的全部内容；会话入口与全局入口的按钮装配（要触达
  外壳的 `pub(super)` 方法）留在 `app/render.rs`——与模态装配同模式（ADR-007）。
- 平台差异按 ADR-029 定档：红绿灯留位是常量（档 1）；Windows caption 按钮、
  Linux 控制按钮是 cfg 变体且门控同门（档 2）；无整套实现差异，不引入平台
  模块或 trait。

## 后果

- 取代 ADR-041 §1（底部动作行）与 §2（设置入口在侧栏顶栏）的位置约定：
  两节描述的位置不复存在，入口收编进窗口顶栏；§3 主题入口形态不变，只改
  锚定方向。ADR-027 的主题选择器归属不变，锚定浮层停靠方向翻转向下。
- 界面骨架从「没有顶栏、终端直接顶到窗口上缘」改为「自绘顶栏常驻」，
  `features.md` 的界面骨架、侧栏与主题入口描述同步更新。
- Windows/Linux 分支（caption 按钮、CSD 请求、`RtlGetVersion` 级别的字体
  细分）在 macOS 机器上只过了 rustc 语法与类型层（同源 gpui API），**未实机
  验证**：`cargo check --all-targets` 与拖拽/双击/按钮命中/边缘缩放的冒烟
  记录进 `docs/context/todo.md` 平台验证，待 Windows/Linux 实机补齐。
- Linux CSD 的阴影、圆角与 `set_client_inset` 精修后置：首版接受平边顶栏，
  compositor 协商回 Server 装饰时顶栏只渲染内容不画控制按钮。
- UI 接线不设单测（与仓库现状一致，ADR-041 同口径）；拖拽状态机与几何往返
  由人工验收守住（`docs/context/manual-acceptance.md`）。

## 修订（2026-11-16，同日二次决策：会话入口回归侧栏）

§1 的左区会话入口迁回**侧栏顶部动作行**：动作行改为「搜索框 + SSH/WSL/本地
图标入口」，输入框实时过滤会话行（标题与来源的大小写不敏感子串匹配，空白串
不过滤），三个入口用内嵌单色 SVG 图标（`svg().data` 内嵌字节、形状只用填充
路径，语义由 tooltip 承载，不引入资产源与打包差异）。顶栏左区只剩拖拽区与
红绿灯留位，中区标题与右区全局入口不变。过滤只影响渲染：行仍携带原始下标，
✕/激活/拖拽排序不受影响；过滤后无命中显示「无匹配会话」占位，搜索文本唯一
住在输入框实体里（编辑经实体观察者唤醒重算）。

同日再修：WSL 入口与本地**同形同图**（同一控制台 `>_`，WSL 会话本就是本地
Shell 的一种，ADR-021），以渲染色区分——WSL 用主题 accent 强调色，其余入口
fg_secondary（色别代替形别；accent 是全主题推导 token，明暗主题对比度已由
主题体系保证，不硬编码品牌色）。SSH 保持服务器机架图形。

## 修订（2026-11-20，Linux Wayland 实机暴露：CSD 边缘缩放落应用侧）

Linux（KDE Wayland / KWin）实机首次验收发现：请求 CSD 后窗口完全无法拖边
调整大小。根因：Wayland 协商出 Client 装饰后，compositor 不再提供缩放边框
（KWin 对 CSD 窗口不画边，GNOME 无 SSD），拖边缩放只能由应用发起
`xdg_toplevel.resize`——协议日志证实整个会话该请求 0 次。gpui（0.3.12 与
0.3.13 同）只提供 `Window::start_window_resize(edge)` 原语、不做边缘命中，
本仓此前也无调用：顶栏的拖拽/双击/控制按钮都齐了，唯独缺缩放。自绘顶栏
之前窗口走默认 Server 装饰（KWin 画边框），所以该能力是在 ADR-057 落地时
才消失的，升级 gpui 不解决。

修复：`views::title_bar::render_resize_edges` 在窗口最顶层（根装配末尾，
浮层在场时边缘仍可缩放）铺四边 + 四角的 8px 透明命中带，悬停给方向光标、
按下 `start_window_resize` 把缩放手势交给 compositor；带内按下
stop_propagation，不与终端选区、顶栏拖拽武装抢事件。挂载条件：
`Decorations::Client` 且未被 compositor 平铺（tiling）、非最大化/全屏、
窗口可缩放。macOS/Windows 不协商出 Client 装饰（前者 trait 默认 Server、
后者边缘由 WM_NCHITTEST 原生命中），命中带保持空转——运行时门控，无 cfg
分叉（ADR-029）。

`set_client_inset` 与阴影/圆角精修**维持后置**：inset 声明的是「应用自绘
装饰带的宽度」，必须与真实装饰视觉（阴影、圆角、贴边内容避让）同做——
单独设置只会把内容区按 inset 收缩、产出一条空带，无用户可感收益。等精修
批次一并落地。

## 修订（2026-11-16，同日三次决策：侧栏会话行瘦身，来源标签撤出）

侧栏会话行去掉第二行来源标签（`LOCAL` / `user@host:port` / `WSL · 发行版名`），
改为标题独占整行：行高恒为两行文本，单行标题垂直居中，最多两行（末行省略）。
会话来源仍由格持有，只退到关闭确认模态里标识受影响的格；侧栏搜索过滤随之只
按显示标题匹配，不再对行上不可见的来源做匹配（修订段「标题与来源」的过滤
口径就此收窄）。🔔 响铃与 ⚠ 日志告警保留，仅以图标内联在标题之前
（语义色着色：响铃 warn 色 / 日志 danger 色，行内垂直居中，不做浮层与
色底；🔔 是 emoji 字形恒彩色、⚠ 默认文本呈现可被主题色染色——色相
区分依赖后者）。行底三态走专属 token：选中 `bg_accent`
（强调色暗调，与 WSL 管理页激活态同款——选中靠色相差异而非明度微调，
`bg_hover` 与 `bg_row` 明度太近难以区分）、未选中 `bg_row`（与侧栏面板底
`bg_panel` 区分，行有形）、未选中悬停 `bg_row_hover`（选中行悬停不再
变化）。「已结束」文字标记一并移除，结束态仍由标题暗色承载。顶栏右区的
`?`/`⚙` 字形入口改用内嵌单色 SVG（快捷键=键盘 / 设置=齿轮），与侧栏
动作行图标同源同框（`svg_icon_entry`），语义仍由 tooltip 承载，渲染色
fg_secondary 跟随主题——字符字形的呈现方式（文本/emoji）无跨平台契约，
图标化后不再依赖。

## 修订（2026-11-23：窗口最小尺寸）

顶栏固定占位约 180px、侧栏最小 140px；此前未设窗口下限，拖窄到该阈值
以下会出现挤压与裁剪。`WindowOptions` 增加 `window_min_size: 480×320`
（顶栏 + 侧栏 + 一列可用终端），不随界面字号缩放，与顶栏/状态栏的定值
像素同一口径。
