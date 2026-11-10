# ADR-031:按键修饰键编码——xterm 修饰参数与 kitty 键盘协议

状态:已接受(2026-10)

## 背景

在 pi 等 AI CLI 里按 Ctrl+Enter 想换行,结果直接触发了发送;Ctrl/Alt+方向键
的按词移动也不生效。链路排查:GPUI 事件里的 ctrl/alt/shift 一路保留到了
`encode_key`,但在编码成终端字节时被丢掉——

- `"enter"` 分支固定输出 `\r`,ctrl/shift+enter 与普通 enter 无法区分;
- 方向键分支同样不看修饰键:ctrl+left 输出 `\x1b[D`(等于普通 left),
  alt+left 输出 `\x1b\x1b[D`(双 ESC,几乎没有程序认识);
- `KeyInput` 不携带 shift,shift+enter 从视图层就传不下来。

pi 的实际行为(源码核实):启动时发 `CSI > 7 u`(push flags)+ `CSI ? u`
(能力查询)+ DA1 哨兵;只有 `CSI ? u` 应答非零 flags 才激活 kitty 增强编码
(ctrl+enter = `\x1b[13;5u`、shift+enter = `\x1b[13;2u`,均识别为换行),
否则回退 legacy 模式——legacy 下 enter 家族只有 `\r`(提交)和 `\x1b\r`
(alt+enter 换行)两种。此前 alacritty_terminal 的 `Config.kitty_keyboard`
走默认 false,协商无应答,pi 一直工作在 legacy 模式。

## 决策

三层编码规则,全部落在 `TerminalModel::encode_key`(字节级编码的唯一归属,
见 ADR-003),视图与 worker 不感知:

1. **Config 显式开启 `kitty_keyboard: true`**。parser(alacritty_terminal
   0.26)自动应答 `CSI ? u` 并维护模式栈,程序 push/pop 后 `TermMode` 的
   kitty 位自动更新,无需额外代码。
2. **kitty disambiguate 模式激活时**,按协议把 legacy 下有歧义的键改报
   CSI u(`CSI codepoint;modifier u`,无修饰省略修饰参数):Esc 带不带修饰
   都报(`\x1b[27u`,否则与转义序列开头无法区分);ctrl/alt+文本键报未按
   shift 的字符码点(`\x1b[99;5u`,ctrl+i 与 tab、alt+b 与「Esc 再 b」从此可
   分);enter/tab/backspace 的修饰组合(含 shift-only)报 functional 码点
   (`\x1b[13;5u`)。**无修饰的 enter/tab/backspace 是协议唯一豁免**,仍发
   `\r`/`\t`/`\x7f`——程序崩溃没来得及 pop 时 shell 里还能敲 `reset`;
   shift-only 的文本键本身就是文本,也走 legacy。口径与 Alacritty 的
   `should_build_sequence` 逐条一致(Esc 无条件;非 shift-only 修饰一律;
   shift-only 只对三个编辑键)。
3. **与模式无关的功能键修饰参数**:方向键/Home/End/Delete/PageUp/Down 带
   ctrl/alt/shift 时输出 xterm 修饰参数形式(`\x1b[1;5D`,参数 =
   1+shift+2alt+4ctrl);SS3 无法表达修饰键,DECCKM 下带修饰也统一 CSI。
   kitty 模式下这些功能键同样保持此形式(协议把 `CSI 1;m X` 列为合法形态
   之一)。legacy 下 Alt 对文本键仍发 ESC 前缀(meta 语义),但功能键的 alt
   已并入修饰参数,不再叠加,避免产生双 ESC 废序列。

配套:`KeyInput` 增加 `shift` 字段。文本键的 shift 已并入 `key_char`
(GPUI 布局处理),该字段只为功能键的修饰参数保留。

## 后果

- pi 等 kitty 协商程序:ctrl/shift/alt+enter 全部正确换行,修饰+方向键
  的按词移动等绑定生效;孤立 Esc 不再依赖超时消歧,ctrl+c 以 `\x1b[99;5u`
  到达(协商了 disambiguate 的程序自己处理中断语义,不再由 tty 发 SIGINT,
  这是协议的既定后果)。
- shell(legacy):ctrl+left/right 发 `\x1b[1;5D/C`,zsh/bash/fish 按词
  移动;ctrl+enter 仍发 `\r`(与 xterm 一致,legacy 协议无法表达,无害)。
- 只实现 flag 1(disambiguate),不实现 flag 2(REPORT_EVENT_TYPES 的
  release/repeat 事件)与 flag 4(REPORT_ALTERNATE_KEYS 的 shifted/base
  layout 键码——GPUI 不给布局信息);`CSI ? u` 的应答由 parser 原样回显程序
  push 的 flags(pi 得到 `?7u`),这两项宣告面大于实现面。pi 的解析把缺失
  release 视为普通 press。若有程序依赖 release 事件再评估。
- 相邻已知问题(本次未动):shift+方向键现在透传 `\x1b[1;2D` 给前台
  程序;若日后想做终端本地选区扩展,需要再决策拦截范围。legacy 下 shift+tab
  仍发 `\t`,xterm 惯例是 `\x1b[Z`(backtab,TUI 反向焦点常用);legacy 下
  ctrl+space 不产生输入(xterm 发 NUL)。

## 修订(2026-11-10):legacy 下纯 alt 的左右方向键发 meta 形态

决策 3 的「功能键 alt 并入修饰参数」在裸 shell 行编辑器上失效(实测
见 [context/alt-arrow-word-motion.md](../context/alt-arrow-word-motion.md)):
zsh 默认键表只绑 `^[b`/`^[f`(meta 形态)→ 按词移动,`\e[1;3D` 无绑定,
zle 把序列尾巴当字面插入(用户看到 "D");后果段「ctrl+left 发
`\x1b[1;5D/C`,zsh/bash/fish 按词移动」的断言对 zsh 不成立——裸 zsh 对
`\e[1;5D` 同样无绑定,需要用户自配。

各终端让裸 shell 开箱即用的两条路线:kitty 靠 shell 集成注入绑定
(只救直接派生的本地 shell,嵌套与 SSH 远端失效);Ghostty 1.4 靠终端
keybind 层把 alt+←/→ 翻译成 `esc:b`/`esc:f`(普适,嵌套与远端同样
生效);iTerm2 的 Natural Text Editing 预设同 Ghostty 形态。

口径变更:ZhimaTerm 是 SSH 客户端,远端 shell 不可能有本地集成,走
终端层翻译——**legacy 下纯 alt(无 ctrl/shift)的 ←/→ 发 `ESC b`/`ESC f`**
(编码层 `terminal::input::encode_key`,本地与远端同时生效);ctrl/shift
组合维持修饰 CSI(meta 形态表达不了 ctrl);kitty disambiguate 激活时
不翻译——协商了键盘协议的程序要标准形式,它们本来就自己解析。
