# Alacritty 可借鉴项

> 归档（2026-10-30）：调研结论已全部落地或定型——落地顺序表除小键盘 APP_KEYPAD
> （GPUI 不区分小键盘按键，缺能力，已移入 `../context/todo.md` 功能池）外全部完成，
> damage 增量快照的否定结论体现在 ADR-003。保留作调研佐证与一手证据索引。

本记录用于 ZhimaTerm 的终端核心与交互设计参考。结论只以两处一手证据为准：我们依赖的
`alacritty_terminal 0.26.0` 源码（本地 vendored），以及 ZhimaTerm 自身实现。
Alacritty 应用层（渲染器、IME、配置装载、hints、visual bell）本次无源码证据，未纳入结论。

## 结论

- **必须补**：`EventListener`。当前用 `VoidListener` 丢弃全部核心事件，终端查询没有应答，属于正确性缺陷。
- **必须补**：输入编码要消费 `TermMode`（`APP_CURSOR`、`APP_KEYPAD`、`FOCUS_IN_OUT`、鼠标上报、kitty 协议）。
- **低成本可补**：选区类型（`Semantic`/`Lines`/`Block` 依赖里已实现，只需接线）。
- **不建议照搬**：`Term::damage()` 增量快照（实测无收益，见下）、渲染器、事件循环实现。

## 核心事件必须被消费

`alacritty_terminal 0.26.0/src/event.rs:14-59` 定义了核心交给 UI 的事件。ZhimaTerm 目前是
`Term<VoidListener>`（`src/terminal/mod.rs`），等于全部丢弃。其中 `PtyWrite` 是**终端查询的应答**，
不回复会让前台程序降级功能或等在超时上：

| 位置 | 触发 | 应答 |
|---|---|---|
| `term/mod.rs:1262` | DA1 `CSI c` | `\x1b[?6c` |
| `term/mod.rs:1284` | 键盘模式 `CSI ? u` | `\x1b[?{mode}u` |
| `term/mod.rs:1337` | DSR 5 / 6 | `\x1b[0n` / `\x1b[{line};{col}R` |
| `term/mod.rs:2090`、`:2147` | DECRQM `CSI ?…$p` | `\x1b[?{};{}$y` |
| `term/mod.rs:2270` | 文本区字符尺寸 `CSI 18t` | `\x1b[8;{lines};{cols}t` |

非应答类事件：`Title`/`ResetTitle`（OSC 0/2，`term/mod.rs:506`、`:2227`）、`Bell`（`:1439`）、
`ClipboardStore`/`ClipboardLoad`（OSC 52，`:1719`、`:1740`）、`ColorRequest`（OSC 10/11，`:1679`）、
`CursorBlinkingChange`、`MouseCursorDirty`。

落地边界：`PtyWrite` 由会话 worker 直接写回 PTY（它是会话内唯一写者，不绕 UI）；
`Title`/`Bell` 作为控制事件上抛给 UI。应答队列的生产者与消费者在同一个线程内
（核心在 `advance` 期间同步回调，worker 每轮排空），因此不需要通道上界。

## 输入编码必须看模式

`TermMode`（`term/mod.rs:53-86`）里的 `APP_CURSOR`、`APP_KEYPAD`、`FOCUS_IN_OUT`、
`SGR_MOUSE`、`UTF8_MOUSE` 与 kitty 协议四位，都需要输入层消费。当前 `input::encode_key`
只看按键本身，`TerminalSnapshot` 也不携带模式，所以设了 `DECCKM` 的程序收到的是 `CSI` 方向键
而不是期望的 `SS3`。待决策：模式随快照给 UI，还是把按键编码整体移进 worker（终端语义只留一份）。

## 可以低成本补的能力

- **选区类型**：`selection.rs:93` 已有 `Simple`/`Block`/`Semantic`/`Lines`，语义选择按
  `Config.semantic_escape_chars` 展开（`term/mod.rs:344-347`）。双击=`Semantic`、
  三击=`Lines`、Alt+拖动=`Block` 只需接线与双击间隔判定。
- **OSC 52 策略**：`Osc52 { Disabled, OnlyCopy(默认), OnlyPaste, CopyPaste }`（`term/mod.rs:369-390`）。
  接事件通道后必须显式决策，尤其是未来 SSH 会话是否允许远端写本地剪贴板；该决策归 ADR-002。
- **配置面**：`Config`（`term/mod.rs:334-367`）暴露 `scrolling_history`（默认 10000，现用默认值）、
  `semantic_escape_chars`、`kitty_keyboard`、`default_cursor_style`、`vi_mode_cursor_style`、`osc52`。
- **同依赖内可复用**：`vi_mode.rs` 的 `ViModeCursor` 动作（可做 copy mode）、
  `term/search.rs` 的 `RegexSearch`（可做查找与 URL 提示）。等有配置层时统一引入。

## 一致性测试方法

`alacritty_terminal` 自带 `tests/ref.rs`（138 行）+ 45 组真实录制语料（vttest、tmux/htop、vim、zsh 补全）：
录制原始字节流 → 回放 → 与期望网格逐格 diff，失败时打印第一处不一致。
该 crate 的 `grid.json` 序列化的是它自己的 `Cell`，不能直接复用；可复用方法与语料，
断言 ZhimaTerm 自己的可观察结果（快照文本、run 的 `columns` 总和等于行宽、选区文本、`display_offset`）。

## 实测否定：不要做 damage 增量快照

`Term::damage()` 在滚动时直接返回 `TermDamage::Full`（`term/mod.rs:467`，`:482-483` 注释说明
display offset 变化按 full damage 处理）。以 crate 自带 45 组真实录制、8KiB 分块回放实测（96×30）：

```
平均损伤 29.7 / 30 行（99%）；按损伤行数估算的增量快照 ≈342µs，全屏快照 353µs
```

shell 场景持续输出必然滚动 → 全屏损伤，收益约 3%。当前的「帧节流 + 全屏快照」是更简单的正确选择；
若日后仍需降低快照成本，方向是快照内部复用缓冲或按行比对，而不是 `damage`。

## 落地顺序与状态

| 顺序 | 内容 | 性质 | 状态 |
|---|---|---|---|
| P0 | `EventListener`：`PtyWrite` 写回 + `Title`/`Bell` 上抛 + OSC 52 写入 | 修正确性缺陷 | 已完成 |
| P0 | 输入模式（`APP_CURSOR` + `FOCUS_IN_OUT`），边界写入 ADR-003 增补 | 修正确性缺陷 | 已完成 |
| P1 | 选区类型 `Semantic`/`Lines`/`Block`（双击/三击/Alt） | 体验 | 已完成 |
| P1 | 鼠标上报（`1000`/`1002`/`1003` + `SGR_MOUSE` + `UTF8_MOUSE` + 滚轮） | TUI 可用性 | 已完成 |
| P2 | 录制流回归（自有确定性语料 + 可指定的外部录制目录） | 防回归 | 已完成 |
| 决策 | OSC 52 策略写入 ADR-002（写入放行、读取拒绝，SSH 前重评） | 安全 | 已完成 |
| P2 | 颜色查询（OSC 4/10/11/12）、`CSI 14t` 像素尺寸、指针形状 | 兼容性 | 已完成 |
| 产品项 | IME（组合可见 + 提交直写，候选窗贴光标） | 输入 | 已完成 |
| 兼容性 | OSC 8 超链接（悬停下划线 + 修饰键点击打开） | 交互 | 已完成 |
| 待定 | `APP_KEYPAD`（GPUI 不区分小键盘按键，缺能力，已移入 todo 功能池）；kitty 键盘协议（归档时已随 ADR-031 落地） | 兼容性 | APP_KEYPAD 未开始 |
| 已落地 | hints：识别纯文本里的 URL（单行、scheme 白名单 http/https/ftp，见 ADR-002 补充决定） | 交互 | 已完成 |

## 未验证

Alacritty 应用层的一切结论本次均无源码证据，包括渲染批处理与字形缓存策略、IME 接入方式、
配置热重载、hints、visual bell，以及其 GPU 渲染方案对 GPUI 场景是否有借鉴价值。
本次取证环境无法访问外部网络，上述内容需要在网络可达时补充一手来源。
