# ADR-061：侧栏与 SFTP 文件列表的键盘导航

状态：已接受（2026-11-23）

## 背景

开发者终端是键盘优先的工具，但侧栏会话行与 SFTP 文件列表此前只有鼠标路径：
切会话必须点侧栏，浏览远端目录必须点行，Esc/Enter 也没有明确的焦点出口。
补键盘路径时不能把方向键漏进 shell，也不能让鼠标点行后焦点留在侧栏。

## 决策

### 1. 侧栏会话列表

- 应用持有 `sidebar_focus`；`session-list` 容器 `track_focus`，列表在聚焦时
  用常驻 1px 透明边框染 `border_focus`（边框一直存在，聚焦只换色，布局不抖）。
- 新增 `FocusSidebar` action：`⌘⇧E`（非 macOS `Ctrl+Shift+E`）把焦点交给列表。
- 列表持焦时：`↑/↓` 循环切换活动会话（只有一个会话时不动）、`Home/End`
  跳首尾；这些都走 `activate_with_focus(..., false)`，只换活动标签、焦点留在
  列表，下一次方向键不会写进 shell。
- `Enter`/`Esc` 调 `activate`，把焦点交给焦点格的终端。
- 鼠标点行在 `on_mouse_down` 里 `window.prevent_default()`，挡住列表
  `track_focus` 的点击聚焦——鼠标路径仍然进终端；点列表空白/清空搜索等
  非行区域才把焦点留在侧栏。
- `FocusSidebar` 进入 `SHORTCUTS` 表，快捷键参考模态自动同步。

### 2. SFTP 文件列表

- 面板根 `track_focus`，焦点落在面板时左缘线染 `border_focus`；
  底栏 SFTP chip 走 `toggle_sftp_panel`（纯开关），新增 `ToggleSftp` action
  （`⌘⇧S` / `Ctrl+Shift+S`）走 `focus_sftp_panel`：面板未开则打开并聚焦，
  已开则只把焦点交给面板，不会把键盘用户想进的面板关掉。
- 列表挂 `ScrollHandle`；`↑/↓` 选择相邻项、`Home/End` 首尾，选中项
  `scroll_to_item` 滚入视口。`Enter/→` 在目录上进入目录（文件上无动作，与
  双击一致）；`Backspace/←` 返回上级；`Delete` 走既有删除确认；`Esc`
  关闭面板并把焦点还给构造期注入的终端焦点句柄。
- 右键菜单开着时列表键不裁决（菜单先收上下/Enter/Esc），对话框开着时交给
  对话框键处理；两者都不会让列表在浮层背后偷偷移动选中项。

### 3. 测试与验收

- 纯逻辑：`active_after_cycle`（已有）与 SFTP `next_selection` 单测覆盖
  首/尾/夹取/空列表。
- 手工验收：`⌘⇧E` 聚焦有边框、↑↓ 切会话、Enter/Esc 回终端；鼠标点行仍
  进终端；SFTP 面板选择/进目录/返回/删除/Esc；主题/字体浮层滚轮与滚动条。

## 后果

- 新增 3 个应用快捷键（侧栏、SFTP、以及此前已有的会话循环）。
- 侧栏与 SFTP 面板各多一个焦点落点；SFTP 面板打开即持焦，终端输入需要
  Enter/Esc 或点击终端回到终端——这是明确的出口，不是陷阱。
- 文件列表的鼠标双击语义保持不变（目录进入、文件无动作）。
