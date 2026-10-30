# ADR-032：终端保真补齐——F 键、文字属性、备用屏滚轮、同步输出超时、光标形状与闪烁、COLORTERM

状态：已接受（2026-10）

## 背景

[终端完备性盘点](../archive/terminal-completeness-audit.md)对照规范体系逐项核对后，
发现解析层（`alacritty_terminal 0.26` / `vte 0.15`）已达 Alacritty 同级，真正的
缺口集中在自研的快照与输入层——核心已经解析出来的状态没有被消费：

1. F1–F12 与 Insert 没有编码分支，键名落到文本兜底、以字面 `f1` 进 PTY（GPUI 的
   macOS 层对这些键不置 `function` 修饰，拦不住）。
2. 斜体/删除线/下划线样式/下划线颜色/隐藏文本在 `TerminalRun` 里没有位置；且
   `intersects(BOLD | BOLD_ITALIC)` 对位或后求交，斜体被渲染成粗体。
3. 备用屏上的滚轮只做本地 `scroll()`，备用屏没有历史，`less`/`man`/`vim` 滚轮无效。
4. 同步输出（2026）只靠 vte 缓冲；vte 的超时只记时刻不比较，宿主没有调 `stop_sync`，
   ESU 不来就冻结到 2 MiB。
5. 快照不带光标形状，`CursorBlinkingChange` 被丢弃：DECSCUSR 全部退化成稳定块。
6. 只声明 `TERM=xterm-256color`，不声明 `COLORTERM`，按它探测的程序降到 256 色。

## 决策

字节级编码与终端语义仍全部落在 `TerminalModel`（ADR-003/031 的归属），worker 与
视图只接线；不改 alacritty_terminal，不新增用户设置项。

### 1. F 键与 Insert 走 xterm 编码

- `insert` → `CSI 2 ~`；F5–F12 → `CSI 15/17/18/19/20/21/23/24 ~`；修饰并入
  `CSI n;m ~`。
- F1–F4 是 PF 键：无修饰固定 SS3 `ESC O P/Q/R/S`，**不随 DECCKM 变**（xterm 行为）；
  带修饰改 `CSI 1;m P/Q/R/S`。
- kitty disambiguate 下这些键保持 legacy 形式（协议把它们列为合法形态，与 Alacritty
  `should_build_sequence` 一致）；Alt 并入修饰参数，不叠 ESC 前缀。
- `is_functional_key` 是这组键的唯一清单，修饰参数与 Alt 处理都以它为准。
- shift+tab 是 backtab，与功能键同列：只有 shift 时发 `CSI Z`（shift 已由 Z 表达，
  不报参数），带 ctrl/alt 发 `CSI 1;m Z`，不随 DECCKM 变；ctrl+space 发 NUL
  （键名 `"space"` 进不了单字符 ctrl 分支，需单列）。kitty disambiguate 下两者
  本就走 CSI u。

### 2. 文字属性进快照

`TerminalRun`/`RunStyle` 增加 `italic`、`strikethrough`、`hidden`、
`underline: UnderlineKind { None, Single, Double, Curly, Dotted, Dashed }`、
`underline_color: Option<u32>`：

- `bold` 只看 `BOLD` 位、`italic` 只看 `ITALIC` 位（`BOLD_ITALIC` 两位都含）。
- `underline_color` 只在真有下划线时记录——孤立的 SGR 58 不该把段切开。
- `is_default_style` 计入全部新属性：带删除线的尾部空格可见，不能当行尾空白裁掉。
- 上游把 SGR 21 当 CancelBold（Linux console 口径），双线只能来自 `4:2`；沿用，不改。

### 3. 渲染：两条绘制路径共用一套装饰画法

GPUI 0.3.9 的 `UnderlineStyle` 只有 thickness/color/wavy，`StrikethroughStyle` 只有
thickness/color，没有双线/点线/虚线。

- 斜体走 `Font.style = Italic`；字体风格经 `font_id` 进 gpui 的布局缓存键，应用侧
  按文本哈希的 `shape_line_by_hash` 不必掺入样式。
- `paint_underline_kind`：Single/Curly 用 `window.paint_underline`，Double 叠两条 1px
  （第二条 +2px），Dotted/Dashed 按格宽切 quad（点线每格两点、点宽 1/4 格；虚线每格一划、
  划宽 1/2 格；周期从段首对齐，相邻格节律连续）。几何抽成纯函数 `underline_segments`。
- 钉位路径（字形不经装饰逻辑）手画下划线与删除线，删除线 y 用 gpui `paint_line` 同款
  公式；流式回退路径改用 `ShapedLine::paint_with_underline_handler`，回调转给同一个
  helper，删除线由 gpui 随字形画出。
- `hidden`：背景、选区、光标照常画，跳过字形与装饰；选区复制仍得到真实文本（xterm 同）。
- 悬停链接只把 `None` 提升为 `Single`，程序给链接文字设的样式保留。

### 4. 备用屏滚轮转方向键（1007）

- `TerminalModel::encode_alternate_scroll`：`ALT_SCREEN ∧ ALTERNATE_SCROLL ∧ 纵向 Wheel`
  时每格一次方向键（形式随 DECCKM）；主屏、横向、程序 `?1007l` 后返回 None。
- worker 在 `wants_mouse()` 早返回之后、本地手势之前接线：程序接管鼠标时仍由
  `encode_mouse` 上报，不走这里。
- 修饰键忽略（xterm 行为）。

### 5. 同步输出超时兜底

- `TerminalModel` 暴露 `sync_deadline()`（vte 记录的时刻）与 `stop_sync()`（回放缓冲）。
- worker 每轮排空输出后检查到期即 `stop_sync` 并置 `dirty`；等待阶段把 deadline 的剩余
  时间纳入定时器（与补帧间隔取较小）——否则空闲期没有任何定时器，超时永远轮不到。
- 超时值沿用 vte 的 150 ms，不另设。

### 6. 光标形状与闪烁

- 快照携带 `cursor_shape: CursorKind { Block, Beam, Underline }`（自研枚举不与上游
  `vte::ansi::CursorShape` 同名，避免别名导入；上游 HollowBlock 只能
  来自配置默认值，核心不会主动产生，并入 Block）与 `cursor_blinking`
  （`Term::cursor_style().blinking`，DECSCUSR 奇数档与模式 12 都会改它）。隐藏仍用
  "没有 run 带 cursor"表达。
- 块状光标沿用 ADR-022 决策 7 的反相填色；竖线/下划线光标不反相，字形之上叠一条
  `cursor_color` 的 bar，厚度 15% 格宽（Alacritty 默认比例）、至少 1px。
- 闪烁节律完全由视图掌握：仅"最新快照要求闪烁 ∧ 窗口激活"时跑 500 ms 半周期循环
  （`ensure_blink`，与 `ensure_clock` 同一 spawn-loop 模式）；键入（含输入法提交）
  时相位复位为亮并从键入时刻重新计时，程序输出不复位——否则 htop 这类刷新快于
  半周期的程序会让光标恒亮（Alacritty 同）；条件消失时置亮并退出；窗口切回且程序
  仍要求闪烁时下一次 render 重新拉起。不提供用户偏好：默认稳定，闪不闪听程序的。
- 窗口非激活：光标不论形状都画成一圈 `cursor_color` 描边的空心框（厚度同 bar），
  不反相、不叠 bar，与 Alacritty 的 `unfocused_hollow` 一致；空心是视图层按
  `window_active` 替换的，核心的 `HollowBlock` 仍并入 Block。

### 7. `COLORTERM=truecolor`

- 本地会话：子进程 env 直接设。
- SSH：在 LANG 之后、`shell()` 之前 `setenv`，与 LANG 共用 `LANG_ENV_BUDGET`（最坏等待
  不叠加），结果不上报——sshd 默认 `AcceptEnv LANG LC_*`，被拒是常态，程序按 256 色
  降级即可，不值得扩 `LangStatus` 契约。

## 后果

- 可见变化：F1–F12/Insert 在 vim、htop、mc 等程序中生效；shift+tab 在 fish/zsh 补全、
  fzf 里反向，ctrl+space 触发 emacs set-mark / tmux 前缀；斜体/删除线/波浪线（vim
  拼写检查）/双线/点线/虚线/下划线颜色各自可辨，SGR 8 隐藏；`less`/`man`/`vim` 未开
  鼠标时滚轮翻页；vim 插入模式竖线光标、程序要求的闪烁在持续输出下节律不乱；窗口
  切走后光标变空心框；同步输出的程序崩溃后画面最多冻结 150 ms；本地 shell 里
  `echo $COLORTERM` 为 `truecolor`。
- 成本：`RunStyle` 多 5 个字段，只在样式真有差异处多切段；闪烁期间每 500 ms 重画一次
  该格视图；SSH 连接多一次 env 往返。
- 已知边界（不在本轮）：kitty 协议 flags 2/8/16 仍未做（ADR-031）；双线第二条固定
  +2px，极小字号下是否贴到下一行 `unverified`。
- 测试：模型层 `function_keys_and_insert_use_xterm_encodings`、
  `backtab_and_ctrl_space_use_xterm_legacy_encodings`、
  `alternate_scroll_turns_the_wheel_into_arrow_keys_only_on_the_alt_screen`、
  `text_attributes_are_carried_into_runs`、`underline_styles_and_color_are_distinguished`、
  `styled_trailing_blanks_are_not_trimmed_as_default`、
  `cursor_shape_and_blinking_follow_decscusr`、
  `synchronized_output_is_buffered_until_esu_or_forced_stop`；渲染层
  `dotted_and_dashed_underlines_are_cut_on_the_cell_grid`。绘制效果、闪烁节律与
  COLORTERM 进[人工验收清单](../context/manual-acceptance.md)。
