# ADR-007：模块边界与依赖方向

状态：已接受

## 背景

重构前 `app.rs`（1485 行）同时承担外壳状态机、两个表单的领域业务、整套模态渲染与下拉组件；
`view.rs`（606 行）混装字体领域、终端网格渲染与外壳框架部件；`settings` 反向依赖 `view`
（取默认字号）。结果是：加一个设置项要动 app.rs，字体逻辑散在消费方里，新代码放哪一目全靠口头惯例。

## 决策

模块按"叶子 → 领域 → 外壳"分层，依赖只允许自上而下：

```
叶子（不依赖任何上层）
  config_paths / input / font / ui（含 ui::input、ui::select）/ sftp
模型层
  terminal（colors、VT 模型）→ input
  session → terminal, input
  session::worker → session, input
  session::transport → sftp, session（SFTP 任务状态机在 sftp_task，见 ADR-005）
领域层（各自闭环的业务/视图）
  connect：连接表单 + 连接模态渲染 + form_auth → ui, connections, session
  settings：持久化与校验 → font, ui, config_paths；settings::form：设置表单 + 模态渲染 → font, ui
视图层（消费领域数据画界面，不定义业务规则）
  views::grid：终端网格布局、命中测试、run 渲染 → font, input, terminal, ui
  views::terminal_view / views::sftp_panel：会话侧视图 → 各自领域（session、sftp）+ ui
  views::panes：标签内分屏布局树 + 格区域渲染（ADR-014）→ views::terminal_view, ui
  views::shell：状态底栏/会话栏/横幅/密钥确认等框架部件 → ui, session
外壳
  app：会话状态机、模态互斥、键分发、渲染装配 → 所有层；只被 main 依赖
```

配套约定：

- 模态渲染全部下沉到表单领域（`connect::render_modal`、`settings::form::render_modal`），
  通过各自的 `ModalActions` 回调集合改应用状态。渲染层只拿 `Rc<dyn Fn>`、不接触应用实体，
  这是渲染下沉后依赖保持单向的关键；app.rs 里每个模态只留一个 `*_modal_actions` 装配函数，
  写明"渲染层的哪个动作回到哪个应用方法"。
- 下拉的展开/收起/选中语义归表单（`SettingsForm::toggle_dropdown` 等），组件 `ui::select`
  只管绘制与回调形状，不知道 `FontField` 之类的业务枚举。
- 终端字体领域（候选、回退链、`FontConfig`、`CellMetrics`）单点在 `font`；
  `DEFAULT_UI_FONT_SIZE` 是 rem 换算基准，留在 `ui` 与组件同居。
- 外壳框架部件（`shell`）与终端网格（`view`）分开：前者围着内容转（多会话、状态、提示），
  后者只消费屏幕快照。
- 格区域渲染（`panes::render_area`）与模态渲染同一模式：布局树是纯逻辑，渲染层经
  `AreaActions` 回调集合改应用状态，app.rs 只留 `pane_area_actions` 装配函数。

## 后果

- 加设置项、改连接表单不再触碰 app.rs；~~app.rs 回到 860 行上下的纯状态机与装配~~
  （行数预期见 2026-10-28 修订：从未达成，也不是目标）。
- 新代码归属可按依赖图判断：改字体进 font，加下拉交互进对应 form，画新外壳部件进 shell。
- 代价：模态渲染多了每帧一次的回调集合构造（约 10 个 `Rc` 分配，与原先每帧 listener 装箱同量级），
  以及每个表单模块 5 行左右的动作类型别名。
- 依赖方向可按 import 检查：领域层出现 `use crate::app` 即违规；settings 不再引用 view。
- 大文件不属于问题本身：`transport/sftp_task.rs`（2000+ 行）是纯任务状态机、零 gpui 依赖、自带测试，
  与 `views::sftp_panel`（面板视图）经 `sftp` 契约解耦，不在本次边界调整范围内。

## 修订（2026-10-18）

目录层级按「领域归领域、视图归视图」重排，依赖方向与分层规则不变：

- `terminal::session` 子树上提为顶层 `session`（四层嵌套变三层）。「terminal」目录
  只留终端语义（VT 模型与颜色），会话协议/worker/传输不再挂在它下面。
- 渲染件收进 `views/`（`grid` 原 `view.rs`、`terminal_view`、`sftp_panel`、`panes`、`shell`），
  领域目录（`terminal`/`session`/`sftp`）保持纯模型。「视图不进领域目录」的规则因此更完整：
  视图不散在顶层、也不混进领域。
- `transfer.rs` 改名 `sftp_task.rs`：原名与端口转发的 `forward.rs` 无法区分，内容是
  SFTP 上传/下载/递归删除状态机（仍在 transport 内，pump 线程约束见 ADR-005）。
- 单文件目录 `sftp/` 拍平为 `sftp.rs`；误导名修正：`session_log` → `output_log`
  （它不参与会话状态，只是输出日志工具）、`paths` → `config_paths`（内容只有配置目录定位）。

## 修订（2026-10-28）

- 回滚转录的格式编解码与逻辑行装配从 `terminal/mod.rs` 拆为子模块
  `terminal::transcript`（Stored* 文件格式、SGR 重编码、LogicalLines 装配器、
  导出/导入方法与 transcript_tests；ADR-039/040）：文件格式升版与 VT 渲染
  修复是两条变更轴，不再落进同一个文件。方法保持 `TerminalModel` 固有实现，
  `StoredLine` 经 `terminal` 原名重导出，消费方（scrollback/worker）零变化。
  纯搬移、零行为变化；键编码（kitty/xterm）仍在 mod.rs，是下一个同型候选。

## 修订（2026-10-28，二）：外壳的预期口径从行数换成职责

- 「app.rs 回到 860 行上下」从未达成（最低 1015，评审时 2974），悬着的行数
  预期会让后续评审误读。修订为职责口径：外壳 = 会话状态机与键分发、模态
  互斥与装配、全体格扇出（`views()` 单点）、工作区编排（快照/恢复/防抖写）、
  平台环境桥接（如 `restored_shell`：session × settings × workspace 的桥接
  决策归壳，挪进任一领域都会引入上层依赖）。**行数不是目标**，可判定的
  归位标准是：加设置项不碰壳（表单自持 + observe）、模态渲染不在壳内、
  逐控件交互不逐个占回调字段、领域词汇不泄漏进外壳。
- 本轮归位记录：保存出口下沉 connect（ADR-048 修订）、重连计划下沉 session
  （ADR-013 修订）、设置表单对齐 observe + ModalActions 缩到语义动作、
  ConfirmCloseModal 下沉 views::shell、`dismiss_anchored` 收编锚定互斥、
  「全体格」三处双层循环收成 `views()`、回滚文件命名契约收口
  `session::scrollback::file_for`、`restored_shell` 自由函数化并补测试矩阵。

## 补记（2026-10-30）：决策一依赖图的清单缺口

决策一的依赖图与 2026-10-18 修订的 views 清单是当期快照，此后新增模块均按
同一分层规则落位，补记如下：领域层 `theme`（ADR-022）与 `recorder`
（ADR-045）；`views` 的 `probe_panel`（ADR-028）与 `history`（ADR-044）；
叶子层的 `keychain`（ADR-018）与 `peb`；`session` 的 `reconnect`（ADR-013
修订）与 `scrollback`（ADR-039/040）；`terminal` 的 `transcript`（本 ADR
修订）。依赖方向与分层规则不变。

## 修订（2026-11-09）：目录按业务域归组

水平分层暴露出结构性错位：类型的归属由「谁渲染它」而非「谁拥有它」决定——
记录数据模型（`TaskItem`/`SessionHistory`）长在 `views/history`、被 `recorder`
反向引用；`pty`/`wsl`/`probe`/`cwd` 是 session 本地传输的机制组成却被当独立
叶子平铺在顶层，业务归属只能靠文档辨认。本修订按业务域归组目录，域内允许
自有「机制 + 视图 + 持久化」，依赖方向规则不变（自上而下）：

- `local/`：本地终端功能——`shells`、`pty`、`cwd`、`peb`、`wsl`、`probe`
  （机制，保持叶子地位，session 内核可依赖）+ `probe_panel`（探针面板视图，
  ADR-028）。
- `recording/`：会话留痕——`output_log`（日志文件机制，ADR-017）、`recorder`
  （记录门面，ADR-045；机制，保持叶子地位，worker 与视图都可依赖）+
  `history`（任务/操作模型与两个全屏模态，ADR-044）。归组同时消解了
  `recorder → views::history` 的反向引用（模型与门面同域）。
- `ssh/`：SSH 连接管理业务域——`connect`（表单与列表弹窗，ADR-026/048）、
  `connections`（保存的连接）、`keychain`（OS 凭据，ADR-018 修订）、
  `forward_form`（ADR-038）。只被外壳与共享视图消费，不引 `views`/`app`。
- `views/` 只留跨业务共享渲染件（服务全部会话类型的 grid/terminal_view/
  panes/shell 等）；单一业务专属的视图随域走。
- 配套：`human_bytes`/`human_rate`/`scaled_1024` 三件套从 `views/shell` 移至
  `ui`（底栏与探针面板共用的展示口径，属「可被任何界面复用」的工具，移走
  消除 `local::probe_panel → views` 反向）。

遗留待修（不属本次，回调反转见后续修订）：`views/shell`、`recording/history`、
`ssh/forward_form` 对 `app`/`views::terminal_view` 的既有桥接调用；`terminal ↔
settings` 常量循环；`ui/terminal → theme` 的 token 定位；`workspace →
views::panes` 的布局格式归属。历史 ADR 与文档中的 `src/pty`、`src/probe` 等
旧路径对应现 `src/local/pty`、`src/local/probe` 等，历史文档不改写。

## 修订（2026-11-09，二）：类型所有权归位

上一修订列出的遗留项中，三处已按「类型归所有者」修复，均为纯搬移、零行为
变化：

- `DEFAULT_SCROLLING_HISTORY` 从 `settings` 归位 `terminal`：它是终端模型的
  默认参数（与 alacritty_terminal 上游默认一致），settings 的同名设置项缺省时
  引用它（settings → terminal；原先 terminal 的测试引 settings，构成循环）。
- `LayoutSpec`/`PaneId`/`SplitAxis` 从 `views::panes` 归位 `workspace`：布局树
  的持久化投影（ADR-037）权威定义归磁盘 schema 的所有者，运行时树
  （`views::panes::PaneTree`）反向引用——解掉 `workspace → views::panes`。
- `CloseTarget` 从 `app` 归位 `views::shell`：关闭确认模态的数据类型随模态走，
  外壳引用它装配目标。

仍未修（规模重估后单独立项）：浮层栈三原语（`overlay_open`/`overlay_close_top`/
`overlay_dispatch`）住在 `app`，而 `ui::dialog`（叶子层）、`views/shell`、
`views/terminal_view`、`recording/history` 都直调。栈本体 `OverlayHost` 虽已在
`ui::overlay`，但 `overlay_open` 联动 `dismiss_anchored`（ADR-044 单点写入）且
经 AppRoot global 写应用实体——下沉为窗口级服务需同步修订 ADR-043/044，属
独立重构而非归位。`ui/terminal → theme` 的 token 定位同留阶段二（appearance
归组时一并处理）。

## 修订（2026-11-09，三）：浮层栈下沉完成

上一条修订里「仍未修」的浮层栈项已落地，取代该段：栈状态从应用实体移到
窗口级 Global（`OverlayRoot`），`overlay_open`/`overlay_close_top`/
`overlay_dispatch` 原语住在 `ui::overlay`，`ui::dialog` 不再依赖 `crate::app`；
入栈互斥经 `overlay_on_open` 钩子回锚定浮层的所有者（app）执行。详见
ADR-044 2026-11-09 修订。app 的 `overlay_*` 方法保留为薄转发，栈渲染装配与
栈顶读取经 `overlay_host(cx)`。

**依赖核对结果**：`ui`、`recording`、`local`、`ssh` 均不再引用 `crate::app`；
`views/*` 仅 `views/shell` 保留一处 `update_app`（WSL 选择后的开会话编排，
属 ADR-044 决策 3 保留的回壳通道）。`ui`/`terminal → theme` 的 token 定位
仍留阶段二（appearance 归组时一并处理）。

## 修订（2026-11-09，四）：connect 按变更轴拆子模块

`ssh/connect` 单文件 3415 行，六条变更轴（表单字段、目标解析、认证持久化、
转发规则、渲染、列表）交汇。按 `terminal::transcript` 的先例拆为子目录，
方法仍是 `ConnectForm` 的固有实现，跨子模块只放开 `pub(super)`：

- `mod.rs`：表单状态机与共享类型（`ConnectForm`、开关/段枚举、`Field`、
  `FormError`、跳板输入集合）；对外 re-export 保持 `connect::ConnectModal`
  等原路径不变。
- `parse.rs`：纯文本解析（端口、目标串、字段合并、`~` 展开）。
- `target.rs`：保存条目 → 会话目标（列表直连路径）。
- `auth.rs`：认证装配（`form_auth`/`form_jump`）与保存计划 `SavePlan`。
- `forward.rs`：转发规则草稿、编辑动作与规则行渲染（ADR-011）。
- `view.rs`：连接弹窗两视图渲染与 `ModalView` 接入（含 `ModalActions`）。
- `tests.rs`：全部单元测试与夹具。

纯搬移、零行为变化；`cargo fmt`/clippy/tests 全绿。

## 修订（2026-11-09，五）：theme 重定位为基础层

决策一的依赖图把 `theme` 列为领域层，但它被 `ui`（叶子）与 `terminal`（模型）
引用，构成「叶子 → 领域」的反向边。核对构成后拆开两类内容：

- `theme` 核心（`ColorScheme`/`Theme`、内置主题表、token 推导、进程级主题
  状态）零业务依赖、只依赖 gpui，是被全层消费的基础数据源 → 重定位为**基础
  层**（叶子地位）；`ui`/`terminal`/`session` 对它的引用是合法的向下依赖。
- `theme::picker`（侧栏主题入口与下拉，ADR-027）依赖 `ui`，是外壳部件 →
  移到 `views::theme_picker`；交互契约与 `PickerActions` 回调形状不变。

同时修正本 ADR 2026-11-09 一修订里「appearance 归组」的设想：`settings` 依赖
`terminal`（模型层），不能与 `theme`/`font` 放进同一目录层，否则又会形成
叶子 → 领域反向边。`settings` 与 `workspace` 留在领域层。

## 修订（2026-11-09，六）：forward_form 落点能力接口化

`ssh::forward_form::ForwardModal` 原持 `Entity<TerminalView>`，把会话视图
类型带进了领域层（域 → 视图的反向依赖）。按依赖倒置改为能力接口：

- 域（`ssh::forward_form`）定义 `ForwardTarget` 契约：`rules`（当前规则）、
  `add`（追加并下发）、`save_identity`（写回保存连接所需的身份，None 表示
  不写回）。模态只持有 `Rc<dyn ForwardTarget>`。
- 外壳（`app`）用 `ViewForwardTarget` 适配 `Entity<TerminalView>` 实现该
  契约，视图类型只出现在外壳；表单的 apply/toggle 动作装配不变。
- `forwards_changed` 改为消费能力接口（规则 + 身份快照），不再读视图实体；
  删除规则后的写回回调同样经适配器。

结果：`ssh` 域对 `views`/`app` 的引用为零。行为等价、纯接口重构；
`cargo fmt`/clippy/tests 全绿。

## 修订（2026-11-09，七）：视图层向上依赖清零

上一条修订后仍有两处向上依赖，按同一「能力注入」模式收掉：

- `recording::history::LoggingModal` 原持 `Entity<TerminalView>` 读记录状态。
  域改为定义 `LoggingTarget` 能力（enabled / log_path / toggle），
  `views::terminal_view` 用适配器实现并注入。`recording` 对 `views` 引用清零。
- `views::shell` 的两个模态曾直调 `crate::app::update_app`：
  `WslModal` 改为构造时注入 `on_pick` 回调；`ConfirmCloseModal` 改为注入
  `confirm`/`cancel` 两个 `WindowAction`，`CloseTarget` 由外壳绑定进动作。
  `views` 对 `app` 引用清零。

结果：除 `main → app` 外，全仓没有任何向上依赖（域/基础层不引 views/app，
views 不引 app）。分层从此可被 import 检查直接验证，也具备升 workspace
crate 做编译边界的前提。

## 修订（2026-11-09，八）：分层升为 workspace crate 边界

上一条修订把向上依赖清零后，分层仍只靠目录与 import 检查。本次把四个中间层
升为 workspace 成员 crate，让 Cargo 依赖图直接拒绝反向引用；根包保留外壳与
视图（它位于依赖图顶端，没有任何 crate 依赖它，再拆拿不到编译期收益）：

```
crates/base      zhimaterm-base       config_paths / font / input / theme / ui / wake
crates/terminal  zhimaterm-terminal   terminal（后端、契约、转录、回放）
crates/runtime   zhimaterm-runtime    sftp / local / recording / session / tests / bin（探针）
crates/ssh       zhimaterm-ssh        connect / connections / keychain / forward_form
根包 src/                              settings / workspace / views / app / main
```

依赖边只允许 `根 → ssh → runtime → terminal → base`（根包另可用全部四层）。

配套改动：

- `wake` 下沉：`WakeupHandle`/`WakeupRead`/`wakeup_pair`/`WakingSender` 从
  `session::transport` 移到叶子模块（现 `zhimaterm-base::wake`），断开
  `sftp → session` 的反向边。探针按 `#[path]` 复用 `poll.rs` 的约定保留：
  poll 与 wake 各自保持 std+libc 自包含，互不引用。
- `tests::support::rendered` 归位为 `TerminalSnapshot::rendered_text()`：测试
  驱动层随 runtime crate 走，模型契约测试不能再引用它；拼法只保留一份。
- 跨 crate 的测试辅助不再用 `#[cfg(test)]` 门控（依赖方按测试编译时不带
  对方的 cfg(test)）：`WakeupHandle::for_test`、`CellMetrics::for_tests`、
  `TextInput::masked`、`TerminalModel::new` 改为常态公开。
- 原先只在 crate 内可见、被新边界切开的项按需升为 `pub`（`BackendKind` 及
  其方法、`colors::reply`、`file_for`/`session_number_of`、`remote_join`、
  `SavedSources` 的字段等）；编译错误就是漏网清单。
- `app.rs`（2707 行）按变更轴拆为 `src/app/` 目录：`mod.rs`（状态结构、构造、
  浮层薄转发）、`sessions.rs`（会话/格生命周期与拖拽排序）、`workspace_io.rs`
  （快照/恢复/防抖落盘）、`modals.rs`（模态与动作装配）、`keys.rs`（裸键分发）、
  `render.rs`（`Render` 实现与侧栏/格区域渲染）。纯搬移，行为不变。
- 根包依赖表随之收缩，只保留外壳/视图/设置/工作区与探针真正使用的 crate。

后果：

- 反向依赖从「import 检查可发现」变为「Cargo 依赖图不可能」；新增 crate 间
  引用必须先改 manifest。
- 代价：跨 crate 需公开原先 crate 内可见的项（API 面略增）；`base` 与
  `runtime` 都依赖 gpui（`ui` 组件与 `recording` 的模态视图），因此 crate 分层
  是**依赖方向**边界，不是「内核零 UI」的证明；`terminal` 代码本身仍零 gpui
  引用。
- `settings`/`workspace`/`views` 留在根包：它们位于依赖图顶端或只被外壳消费，
  再拆 crate 没有编译期收益。

## 修订（2026-11-09，九）：runtime 按域拆分与 connections 改名

修订八的 `zhimaterm-runtime` 是层名不是域名：约 2.1 万行里装着三个域
（会话 1.0 万、本地终端 0.45 万、会话留痕 0.13 万），而 `zhimaterm-ssh`
名义上是 SSH 域、装的却是连接管理（表单/持久化/凭据），SSH 协议本体在
runtime 的 `session::transport::ssh`——两个名字都不见名知意。核对发现
`local`/`recording` 对 `session`/`terminal` 零引用（只依赖 base），早已是
事实上的独立 crate，只差分家。本次按域拆分并改名，依赖方向规则不变：

```
crates/local        zhimaterm-local        本地终端域（pty/shells/cwd/wsl/peb/probe + probe_panel + pty 探针）
crates/recording    zhimaterm-recording    会话留痕域（output_log/recorder/history）
crates/session      zhimaterm-session      会话域（原 runtime 余部：协议/内核/传输（SSH 协议本体、
                                           SFTP 任务）/sftp 契约/e2e tests/ssh-sftp 探针；
                                           session/ 目录拍平到 crate 根，路径去一层）
crates/connections  zhimaterm-connections  SSH 连接管理域（原 zhimaterm-ssh 改名：
                                           connect/connections/keychain/forward_form）
```

依赖边：`根包 → connections → session → terminal → base`；`session → local`
与 `session → recording`（机制保持叶子地位）；根包另可用全部 crate。

配套改动：

- **history 依赖倒置**：`recording::history` 原引 `session::sftp` 的
  `TaskProgress`/`TaskOutcome`（若 recording 成 crate 会与 `session →
  recording::recorder` 构成循环）。进度/终态改为 recording 自有的留痕投影
  类型，由唯一写入方（SFTP 面板，在根包）折算——同 ForwardTarget/LoggingTarget
  的能力注入先例，`recording → session` 引用清零。
- `isolate_shell_environment`（测试子 shell 的 ZDOTDIR 隔离）归
  `zhimaterm_local::pty` 常态公开：它是本地 PTY 域的测试基建，session 的
  e2e 驱动跨 crate 引用。
- 跨 crate 测试辅助按修订八口径常态公开：`recorder::attach_pane_with_log`/
  `test_pane`、`output_log::from_file_for_test`、`probe::ProbeHandle::new`。
- 探针各随其主：`pty_probe` → local，`ssh_sftp_probe` → session（`#[path]`
  复用 `poll.rs` 的约定不变，仍在同一 crate 内）；e2e `tests/` 留在 session
  （以正常依赖引 local/recording）。
- 顺带清除：runtime 的 `regex` 依赖无使用（历史残留），随拆分删除；
  `windows-sys` 的 `Win32_Security`/`Win32_System_Kernel` feature 全仓零
  使用，不再带入 local；`Win32_System_Environment`（peb 的 GetCommandLineW）
  归 local。
- 原 `session/mod.rs` 的内联单测改名 `session_tests`（crate 根的 `tests`
  名留给 `tests/` 目录的 e2e 驱动）。

后果：

- 「SSH 协议在哪」的答案是 `zhimaterm-session::transport::ssh`（会话域的
  SSH 传输），「连接怎么管理」在 `zhimaterm-connections`——两侧名字都为真；
  `local`/`recording` 与 `session` 的单向从模块纪律升级为 Cargo 强制。
- 代价：跨 crate 公开面略增（清单见上）；workspace 成员 5→7；Windows/Linux
  平台分支搬家后需在对应平台各过一次 `cargo check --all-targets`（本次在
  macOS 完成 fmt/clippy/test，跨平台检查待办）。
