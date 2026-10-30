# ADR-022：主题系统

日期：2026-10-17
状态：已接受

## 背景

界面颜色在实现过程中以硬编码字面量散落在约 10 个文件、119 处（部分文件后来收敛成
文件级常量，如 `views/sftp_panel.rs` / `card.rs` / `ui::input`），终端调色板是 `terminal/colors.rs`
里的常量表。没有主题能力，也无法统一调整观感。

设计前对照了 Tabby 的主题模型（配色方案与 UI 主题分离、UI 从配色方案推导）、
Windows Terminal 的内置 schemes 表与 iTerm2-Color-Schemes 的数据格式。

## 决策

### 1. 一套主题 = 终端配色方案，界面颜色全靠推导（Tabby 模式）

主题作者只给 20 个数：前景、背景、光标 + 16 ANSI 色（`theme::ColorScheme`，
另带一个暂不启用的 selection 槽位）。界面的 ~45 个颜色（`theme::UiTokens`：背景分层、
边框、文字梯度、语义色、叠加色）全部由 `derive_tokens` 从方案推导：

- 深浅判定：背景亮度 < 前景亮度即深色，决定分层方向（深色主题根底更暗、交互元素
  逐级向前景色提亮；浅色主题方向相反）。
- 文字梯度与中性分层：前景/背景线性插值的固定档位。
- 语义色取 ANSI 的 normal 档（红/黄/绿）——主题作者按各自主底校过可读性；
  界面强调色取显式 `ui_accent`，缺省回落 ANSI 亮青。
- 叠加色（搜索高亮、滚动标记、输入选区、模态遮罩）= 语义色加固定 alpha，
  gpui `rgba` 的 `0xRRGGBBAA`（alpha 在低字节）。

理由：新主题零 UI 成本；界面与终端永远同一观感（现状就是 One Dark 一套色系的手工版）。

### 2. Default 主题携带手调 token，其余主题全走推导

`UiTokens` 支持主题级覆盖（`Theme::ui`）。Default 的手调值与历史硬编码逐值一致，
是观感的权威基准；第三方主题一律 `ui: None`。`derive_matches_default_baseline` 测试
约束推导算法对 Default 的输出每通道偏差 ≤32（两个手挑的中间调 ≤48），防止档位漂移
连带第三方主题观感漂移。不用「全手写 33 套 token」：每加一个主题都要手调一套 UI 色，
成本与一致性风险都不可接受。

### 3. 内嵌 33 套、不支持自定义与导入

编译期静态表（`theme/builtins.rs`）：Default + 32 套第三方（Ayu Dark/Light/Mirage、
GitHub 九变体、Catppuccin ×4、TokyoNight ×3、Rosé Pine ×3、Gruvbox ×2、Everforest ×2、
Kanagawa ×2、One Half ×2、Nord、Dracula），数据取自公开主题集（Ayu 三变体与 GitHub
九变体来自 terminalcolors.com，其余取自 iTerm2-Color-Schemes 的 Windows Terminal
导出，槽位一一对应）。
不做 TOML/JSON 解析器、不做主题目录：解析器、文件容错与校验都是只为"自定义"存在的
成本，而这不是当前需求。第三方主题数据许可为集合 MIT、单主题版权归各自作者，
README 附来源与许可说明。

### 4. 进程内主题状态是一个 gpui Global，`set_theme` 是唯一写入方

`theme::init`（启动，按 `settings.theme` 装载，未知名字回落 Default）与 `theme::set_theme`
（切换/预览）写 Global；界面渲染经 `theme::tokens(cx)` 只读。token 以 `Arc` 传递，
渲染函数显式收 `&UiTokens` 参数（无 cx 的自由函数）或内联 `tokens(cx)`（组件方法）。

### 5. 会话经命令通道收方案引用，下一帧生效

`SessionCommand::SetTheme(Arc<ColorScheme>)`：worker 侧 `TerminalModel::set_scheme`
换引用即可——网格存的是语义色，每帧快照才解析成 RGB。不重放输出、不重建会话。
spawn 后立即下发一次当前方案（模型以 Default 起跑，首帧前命令必然已处理）。
队列满时丢弃：主题是视觉状态，下一次切换自然覆盖。

### 6. 切换入口：侧栏底部独占剩余宽度；点击切换、点外部收起

入口显示当前主题名，点击展开下拉（向上弹出：入口贴侧栏底缘，向下会溢出窗口）。
下拉项带色板（主题背景叠红/绿/蓝三点）。**点击一项立即切换并落盘，下拉保持展开**：
连续点几项比对观感是主要用法；收起只由点击下拉之外（全窗口遮罩）触发。
不做悬停预览——整屏换肤跟着指针晃动太刺眼，试色交给「点击后不收起」的连续比对。

> 2026-10-21 调整：入口改为侧栏底部动作行末尾的三点色板，不显示主题名；
> 展开方向、点击即切换、保持展开的交互不变。见 ADR-041。

### 7. 光标用主题光标色填充；选区维持反相

光标色无对比度风险，改用 `scheme.cursor`（颜色查询 OSC 12 同步回答它）。
选区维持既有的反相绘制：第三方主题的 selectionBackground 差异大（如 Catppuccin Mocha
近白），着色后前景文字对比度不可控，反相在任何主题下天然可读。selection 槽位保留在
数据结构里，将来需要再启用。

## 后果

- 界面加新颜色必须走 `UiTokens` 新增字段 + 推导档位，不再允许硬编码字面量。
- 浅色主题（Latte / Day / Light 系）已可用；逐屏观感需人工验收（推导算法保证的是
  分层关系，不是每屏的艺术效果）。
- 内嵌表新增主题只需加一个 `Theme` 常量；若某主题推导观感不佳，可给它单独配
  手调 `ui`（机制已就位）。
- 修改推导档位会同时影响全部第三方主题，基线测试（决策 2）是唯一守门员。

## 相关

- ADR-003（渲染边界）、ADR-008（网格立即渲染）、ADR-016（本地格无底栏——主题入口
  挂在外壳侧栏，不依赖底栏存在）。
