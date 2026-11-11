# ADR-053：侧栏自动标题改用工作目录

状态：已接受（2026-11-11）。

## 背景

侧栏自动标题的兜底是 `会话 N` 显示编号（ADR-042）：本地 shell 默认不设置
OSC 标题，所以本地会话的侧栏实际上常年显示「会话 1」「会话 2」……编号随
使用递增，既不传达会话在做什么，也让用户觉得「一直在涨」。

工作目录的数据链路已经存在（ADR-039）：worker 在有输出后采样上报
`CwdChanged`，视图状态持有 `cwd`，变化即触发外壳重画与工作区落盘。目录比
编号更有信息量——它是用户区分「哪条工作线」的直接线索。本次废弃显示编号
机制（取代 ADR-042），自动标题的兜底改为工作目录。

## 决策

### 1. 自动标题优先级：OSC 标题 > 工作目录 > 来源标签

`TerminalView::label()`（ADR-047 的优先级链更新为
**用户起的名字 > OSC 标题 > 工作目录 > 来源标签**）：

- **OSC 标题仍第一优先**：前台程序（vim、ssh 等）设置的标题比目录更准地
  回答「里面在跑什么」，保留既有行为。
- **工作目录**：取视图状态里的 cwd（ADR-039 链路，`cd` 即跟随刷新）。home
  前缀缩写：home 本身显示 `~`，home 下的路径用平台分隔符接在 `~` 后
  （Unix `~/owner/zhimaterm`、Windows `~\projects`，不混排）；其余路径
  原样显示（Windows 自然带 `C:\Work` 盘符）。home 取
  `config_paths::home_dir()`。
- **Windows 大小写**：cwd 读自目标进程 PEB（ADR-039），cmd 的 `cd` 保留
  手打大小写而 Windows 路径本身大小写不敏感——home 前缀匹配逐组件小写
  比较（ADR-029 档 2 的 cfg 变体），`cd C:\USERS\FANG` 也能缩写成 `~`。
- **来源标签兜底**（无 cwd 的会话）：`SessionTarget::description()`——
  SSH 显示 `user@host:port`，WSL 显示发行版名，本地显示 `LOCAL`。

### 2. 废弃 ADR-042 的显示编号机制

`label_created` 计数、`display_number` 传参与字段、关空列表归零、工作区
`label_created`/`label_number` 字段与恢复回退，整链删除：

- **持久化编号不动**：`created` 与工作区叶的 `number` 继续只增不复用，
  仍是回滚文件键与叶标识（ADR-039 不变）。
- 分屏标签的标题仍取聚焦格（ADR-047 不变）；窗口标题仍只跟 OSC 标题走，
  不受本次影响。

### 3. 工作区 schema 删字段的兼容口径

删除 `Workspace.label_created` 与 `SavedNode::Local/Wsl.label_number`，
**不升 `WORKSPACE_VERSION`**：

- 新代码读旧文件：serde 默认忽略未知字段，照常读取；
- 旧代码读新文件：这些字段原本就带 `#[serde(default)]`，缺失时回退 0 →
  恢复到持久化编号（旧构建的 ADR-042 行为），可读。

双向兼容是「忽略」而非「语义变更」，不构成升版本理由。

## 后果

- 本地会话默认显示目录（home 显示 `~`），`cd` 时侧栏实时跟随；不再出现
  递增编号。
- 兜底边界：WSL 的工作目录在子系统里、宿主侧采集不到（ADR-021/037 既有
  边界），恒显示发行版名；SSH 会话没有 cwd 采样，无名（ADR-048）无标题时
  显示 `user@host:port`；本地会话从启动到首个 `CwdChanged`（首屏输出后
  采样，秒级）之间短暂显示 `LOCAL`，恢复的格有 `set_remembered_cwd`
  预填、无此空窗。
- ADR-042 整体废弃（文内修订段标注）；ADR-047 §1 的优先级链由本文取代。
- 单测：`sidebar_cwd` 纯函数（home 本身 / home 子目录 / 非 home /
  home 不可得）与工作区往返（无编号字段）。Windows 的大小写不敏感比较与
  `~\` 接法只在 Windows 编译，macOS/Linux 上无法验收（需 Windows 实机
  `cargo check --all-targets` 与手工验证）。UI 编排（cd 跟随、OSC 优先、
  兜底文案）按仓库惯例不设单测，手工验收项见 `docs/context/manual-acceptance.md`。
