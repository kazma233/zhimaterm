# 终端完备性盘点：规范体系、特性清单与自研层缺口

> 已归档（2026-10-21）：6 项自研层缺口已由 ADR-032 全部落地；此后的
> 滚动历史上限（ADR-035）与全彩回滚（ADR-040）不反映在正文中。
> 本文保留当时的证据与排序依据。

状态：盘点结论（2026-10）。6 项自研层缺口的修复决策与落地见 ADR-032。

结论只以一手证据为准：本仓库源码、依赖 `alacritty_terminal 0.26.0` 与 `vte 0.15.0`
的本地源码（`~/.cargo/registry/src/.../alacritty_terminal-0.26.0`、`vte-0.15.0`）。
标注 `unverified` 的条目需要实机验证，未写进结论。

## 结论

- 终端核心复用 `alacritty_terminal`，**解析与协议应答达到 Alacritty 同级**：日常
  shell 与 TUI 用到的 ECMA-48/DEC/xterm 序列都已覆盖，设备查询有应答，输入编码
  按模式（DECCKM、kitty disambiguate、鼠标三种编码）区分，测试细。
- 真正的缺口不在解析层，而在**自研的快照/输入层没有消费核心已经解析出来的状态**：
  F 键与 Insert 无编码、斜体/删除线/下划线样式/隐藏文本被丢弃、备用屏滚轮
  （1007）未转方向键、同步输出（2026）无超时兜底、光标形状与闪烁未渲染、
  `COLORTERM` 未声明。6 项都是自研层几行到几十行的改动。
- 与 Kitty/WezTerm/iTerm2 一级的差距（图像协议、BiDi、kitty 键盘协议全量）属于
  设计决策或依赖能力边界，不在本轮范围。

## 一、控制台规范体系

没有一份覆盖现代终端的完整正式标准，实际是分层的事实标准：

| 规范 | 地位 | 覆盖范围 |
|---|---|---|
| ECMA-48 / ISO 6429（前身 ANSI X3.64） | 唯一正式标准 | 控制函数骨架：C0/C1、CSI、SGR 基础 8 色、光标/擦除/滚动。不含鼠标、OSC 扩展、256 色 |
| DEC VT100–VT520 手册（vt100.net） | 事实标准 | DEC 私有模式（`CSI ?`）、滚动区域 DECSTBM、DECSC/DECRC、DA、DCS、Sixel；VT 等级 1–4 即兼容性阶梯 |
| xterm ctlseqs（Thomas Dickey） | 现代扩展的事实规范 | 256 色/真彩、鼠标 1000–1006/1015、备用屏 1049、bracketed paste 2004、焦点 1004、OSC 0/2/4/10–12/52/104–112、窗口操作 `CSI t`、modifyOtherKeys、XTGETTCAP |
| terminfo / termcap + `TERM`、`COLORTERM` | 能力发现 | 程序据此判断终端支持什么；终端通过 `TERM` 自我声明 |
| Unicode UAX #11、UAX #29 | 宽字符、字形簇 | CJK/emoji 占两格、组合字符归前导格 |
| 社区规范（非正式） | 现代终端扩展 | kitty 键盘协议、kitty graphics、OSC 8 超链接、2026 同步输出、OSC 133 shell integration、OSC 9;4 进度、iTerm2 图像（OSC 1337）、BiDi in Terminals、2027 字形簇、2048 带内 resize |

一致性测试：vttest（DEC 系经典）、esctest（iTerm2 作者的转义序列测试套件）、
`alacritty_terminal` 自带的 45 组真实录制语料——本仓库已用后者做回放回归
（`src/terminal/replay.rs`、`src/tests/perf_acceptance.rs`）。

## 二、职责划分：依赖负责什么，自研层负责什么

```
PTY 字节 → vte::ansi::Processor（解析）→ alacritty_terminal::Term（网格/模式/事件）
        → TerminalModel::snapshot（自研：按 run 快照）→ views/grid.rs（自研：GPUI 绘制）
按键/鼠标 → TerminalModel::encode_key / encode_mouse（自研：按 TermMode 编码）→ PTY
Term 事件 → TermRequestProxy（自研：转 TermRequest）→ worker 写回 PTY / 上抛 UI
```

- 依赖负责：状态机、网格、模式位（`TermMode`）、SGR 属性落到 `Cell.flags`、
  查询应答文本、滚动历史、resize 重排、选区、搜索。
- 自研层负责：决定哪些状态进快照（`TerminalRun`/`RunStyle`）、怎么画、按键与鼠标
  按什么模式编码、哪些核心事件被消费。**本盘点的 6 项缺口全部落在自研层。**

## 三、特性对照清单

### 3.1 已达到

| 领域 | 现状 | 证据 |
|---|---|---|
| VT 解析 | ESC/CSI/OSC/DCS/APC/PM、UTF-8、滚动区域、原点、自动折行、制表、插入模式、LNM | vte `ansi.rs:884-967` 模式表；`alacritty_terminal` ref 测试 |
| 私有模式 | 1/3/6/7/12/25、1000/1002/1003/1004/1005/1006/1007/1042/1049、2004、2026、kitty 键盘 | vte `ansi.rs:939-967`、`term/mod.rs:53-120` |
| 颜色 | SGR 8/16/256/真彩（38;2）、bold/dim/反相、OSC 4/10/11/12 改写与查询、104/110-112 重置 | `src/terminal/colors.rs`、`src/terminal/mod.rs:851-854` |
| 鼠标上报 | 1000/1002/1003 三档；legacy/UTF-8(1005)/SGR(1006) 三种编码；滚轮、修饰键 | `src/terminal/mod.rs:874-970` + 5 个测试 |
| 键盘编码 | DECCKM（SS3/CSI）、xterm 修饰参数、kitty disambiguate（CSI u）、Alt=ESC 前缀、Ctrl 控制字节 | `src/terminal/mod.rs:770-849`、ADR-031 |
| 查询应答 | DA1、DSR 5/6、CPR、DECRQM、`CSI 14t`/`18t`、`CSI ? u`、OSC 颜色查询 | `src/terminal/mod.rs:38-103`、测试 `device_queries_are_answered_as_requests` |
| 粘贴 | bracketed paste（2004）、剥 ESC 防注入 | `src/terminal/mod.rs:750-761` |
| 焦点 | 1004 上报 | `src/terminal/mod.rs:862-870` |
| 剪贴板 | OSC 52 只写不读（ADR-002） | `src/terminal/mod.rs:444` |
| 超链接 | OSC 8 + 纯文本 URL（scheme 白名单 http/https/ftp） | `src/terminal/mod.rs:181-215`、`1086-1132` |
| 文本 | 宽字符、组合字符、emoji（unicode-width）；宽字符独立成段居中绘制 | `src/terminal/mod.rs:1023-1043` |
| 网格 | 滚动历史（10k，上游默认）、resize 重排、备用屏、选区四种语义、搜索 | 上游 + `src/terminal/mod.rs:559-745` |
| 回归 | 自有确定性语料 + 外部录制回放、快照不变量断言 | `src/terminal/replay.rs` |

### 3.2 自研层丢掉的（本轮要修的 6 项）

每项给出：现象、根因位置、核心已提供的能力、标准行为。

**1. F1–F12 与 Insert 没有 VT 编码**

- 现象：`encode_key` 的分支表只有方向键/Home/End/Delete/PageUp/PageDown
  （`src/terminal/mod.rs:797-825`）。F 键与 Insert 落到兜底分支
  `key.key_char.unwrap_or(key.key)`（`:834-839`），把键名当文本发出——敲 F1
  打出字面 `f1`。
- 上层过滤只拦 `platform`/`function` 修饰键（`src/input.rs:100-104`），而 GPUI
  的 macOS 事件层对 F1–F12 显式**不置** `function`（`gpui-pre-macos-0.3.9/
  src/events.rs:354-356` 排除了 NSFunctionKey 字符区间；Linux/Windows 层硬编码
  false），F 键确实一路漏到兜底分支。三平台键名一致：小写 `f1`…`f12`、`insert`。
- 标准编码（xterm）：F1–F4 为 SS3 `ESC O P/Q/R/S`，带修饰改 `CSI 1;m P`；
  F5–F12 为 `CSI 15/17/18/19/20/21/23/24 ~`，带修饰 `CSI n;m ~`；Insert 为
  `CSI 2 ~`。kitty disambiguate 下这些键本就是 CSI 序列，不需改报 CSI u。

**2. 斜体、删除线、下划线样式变体、下划线颜色、隐藏文本在快照层被丢弃**

- 现象：`RunStyle`/`TerminalRun` 只有 `bold`、`underline` 两个样式位
  （`src/terminal/mod.rs:130-143`、`:404-411`）。快照循环
  （`:1012-1022`）用 `intersects(BOLD | BOLD_ITALIC)` 判粗体——这是对位或后求交，
  italic-only 的格也命中，**斜体显示成粗体**，没有任何斜体字体切换；
  `ALL_UNDERLINES` 折叠成一个 bool，undercurl/双线/点线/
  虚线全部退化成单下划线；`STRIKEOUT`、`HIDDEN` 完全没读，隐藏文本（SGR 8）会
  原样显示；SGR 58 下划线颜色没读，下划线永远跟文字同色。
- 绘制处 `strikethrough: None`、`wavy: false` 硬编码（`src/views/grid.rs:343-351`），
  字体只切 `FontWeight::BOLD`，没有 `FontStyle::Italic`。
- 核心已提供：`Flags::ITALIC/STRIKEOUT/HIDDEN/DOUBLE_UNDERLINE/UNDERCURL/
  DOTTED_UNDERLINE/DASHED_UNDERLINE`（`cell.rs:16-33`）、`Cell::underline_color()`
  （`cell.rs:197`）。
- 影响：vim 拼写检查的波浪线、`ls`/git 的斜体与删除线、`tput sitm` 全部不可见。

**3. 备用屏滚轮（alternate scroll，模式 1007）未消费**

- 现象：worker 收到滚轮时，程序没开鼠标上报就只做本地 `terminal.scroll()`
  （`src/session/worker.rs:432-447`、`:549-560`）。备用屏没有滚动历史，
  `less`/`man`/`vim`（未开鼠标）里滚轮完全无效。
- 核心已提供：`TermMode::ALTERNATE_SCROLL` 默认开启（`term/mod.rs:72`、`:117`），
  程序可用 `CSI ? 1007 h/l` 开关（`:1980`、`:2032`），DECRQM 可查（`:2072`）。
- 标准行为（xterm/Alacritty/kitty 一致）：备用屏 + 1007 开 + 未开鼠标上报时，
  滚轮上/下各格转成一次方向键上/下（按 DECCKM 选 CSI 或 SS3 形式）；主屏不变。

**4. 同步输出（2026）没有超时兜底**

- 现象：vte 收到 BSU（`CSI ? 2026 h`）后缓冲全部输出，直到 ESU 或缓冲满 2 MiB
  （`SYNC_BUFFER_SIZE`，`ansi.rs:39`、`:375-377`）。`StdSyncHandler::pending_timeout`
  只查 `timeout.is_some()`（`ansi.rs:469-471`），**不比较时间**；超时（150 ms，
  `ansi.rs:36`）后需要宿主主动调 `Processor::stop_sync()`（`:315`）。本仓库没有
  任何 `sync_timeout`/`stop_sync` 调用点。
- 影响：程序在 BSU 之后崩溃、被 kill、或 SSH 断流，画面冻结在上一帧，直到攒满
  2 MiB 才强制冲刷。DECRQM 对 2026 应答 Reset（`term/mod.rs:2084`，表示"认识此
  模式"），neovim/zellij 等会据此启用同步输出，隐患实际可触发。
- Alacritty 应用的做法：事件循环每轮读 `processor.sync_timeout().sync_timeout()`
  的 deadline，到期调 `stop_sync`。

**5. 光标形状与闪烁未渲染**

- 现象：`TerminalSnapshot` 没有光标形状字段（`src/terminal/mod.rs:379-393`），快照
  只检查 `CursorShape::Hidden`（`:984`）；视图永远画块状光标。
  `Event::CursorBlinkingChange` 被显式丢弃（`:97`）。
- 核心已提供：`Term::cursor_style() -> CursorStyle { shape: Block|Underline|Beam|Hidden,
  blinking: bool }`（`term/mod.rs:942`、vte `ansi.rs:821-838`）；DECSCUSR
  （`CSI Ps SP q`）与模式 12 的变化都会发 `CursorBlinkingChange`
  （`term/mod.rs:834`、`:1856`、`:1990`、`:2039`）。
- 影响：vim/neovim 插入模式的竖线光标、`CSI 4 SP q` 下划线光标不生效。

**6. `COLORTERM=truecolor` 未声明**

- 现象：本地会话只设 `TERM=xterm-256color`（`src/session/transport/local.rs:62`），
  SSH 以同名申请 PTY（`src/session/transport/ssh.rs:145`）。核心支持 SGR 38;2
  真彩，但按 `COLORTERM` 探测的程序（neovim、bat、delta、大多数 TUI 框架）会
  降级到 256 色。
- 约束：SSH 侧经 `channel.setenv` 下发受 sshd `AcceptEnv` 限制，OpenSSH 常见
  默认只放行 `LANG LC_*`，`COLORTERM` 多数服务器会拒绝。本仓库已有 LANG 下发
  与结果上报的机制（ADR-019，`ssh.rs:160-166`）。

### 3.3 未实现（设计决策或能力边界，不在本轮范围）

| 项目 | 性质 | 依据 |
|---|---|---|
| Sixel / kitty graphics / iTerm2 图像 | 设计决策：后置 | requirements §3.3 末段 |
| 双向文本（BiDi） | 依赖能力：alacritty 系不支持 | 上游无 bidi 处理 |
| kitty 键盘协议 flags 2/8/16（松键上报、alternate keys、associated text） | 与 Alacritty 应用同水平，只做 flag 1 | ADR-031、`src/terminal/mod.rs:780-795` |
| 小键盘应用模式（APP_KEYPAD） | 平台能力：GPUI 不区分小键盘按键 | alacritty-lessons 待定项 |
| 1015 urxvt 鼠标编码 | 罕见，xterm 自身也标注过时 | — |
| OSC 133 shell integration、OSC 9;4 进度 | 未立项 | — |
| OSC 52 读取 | 安全决策：拒绝 | ADR-002 |
| 滚动历史 10k 固定 | 已暴露配置（ADR-035）：设置面板 0–100,000，默认 10,000 | ADR-035 |

## 四、未验证项（unverified）

- 连字（ligature）：`grid.rs` 按字形簇钉位每格（`cell_start_bytes`），跨格连字
  是否成形未实测。
- 彩色 emoji 字体回退在三平台的实际表现。

## 五、修复优先级（已按 ADR-032 全部落地，此处保留当时的排序依据）

按收益与改动量排序：

1. 样式快照（第 2 项）与备用屏滚轮（第 3 项）：日常可见、改动集中在
   `TerminalRun`/`grid.rs` 与 worker 一处分支。
2. F 键与 Insert（第 1 项）：`encode_key` 加分支表。
3. 光标形状与闪烁（第 5 项）：快照加字段，视图按形状绘制；闪烁需要视图侧定时器。
4. 同步输出超时（第 4 项）：worker 轮询 deadline，到期 `stop_sync`；低概率但体验差。
5. `COLORTERM`（第 6 项）：本地一行；SSH 侧沿用 LANG 的尽力下发路径。
