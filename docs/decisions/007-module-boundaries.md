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
