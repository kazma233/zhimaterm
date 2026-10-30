# ADR-033：⌥ 作为 Alt——option 修饰的语义与中文输入

状态：已接受（2026-10）

## 背景

两个现象同源：

1. macOS 上按 ⌥+字母，终端里出现 `<ffffffff>` 这样的怪串（zsh 显示的，不是终端画的）。
2. 用中文输入法时，拼音只有第一个字母出现在界面上，后续字母不出现。

根因不在解析层，也不在编码层，而是**输入层把 macOS 合成的特殊字符当成了按键文本**。

### 现象 1 的完整链路

- macOS 的 `UCKeyTranslate` 在 option 修饰下把字母键映射成特殊字符：ABC 布局下
  ⌥+b = U+222B (∫)、⌥+f = U+0192 (ƒ)。GPUI 的 `parse_keystroke` 用
  `chars_for_modified_key(keyCode, OPTION_MOD)` 填 `key_char`
  （`gpui-pre-macos/src/events.rs:458-467`），所以 `key_char` = "∫"，
  而 `key` 字段用不含 option 的 `chars_ignoring_modifiers` = "b"。
- zhimaterm 的 `encode_key` 在 legacy 分支用 `key_char` 作字节
  （`src/terminal/mod.rs:966-971`），于是把 `ESC` + `∫` 发给前台程序。
- zsh 5.9 收到 `ESC` + 多字节 UTF-8，无法把它解码成一个按键，ZLE 把无法解码的
  meta 序列显示为 `<ffffffff>`（键字符 'f' 的十六进制）。伪终端实测复现：
  喂 `ESC e2 88 ab` 给 zsh，输出正是 `\x1b[7m<ffffffff>\x1b[27m`。
- kitty 模式（`CSI > 1u`）下走 `text_codepoint(&key.key)`，取的是不含 option 的
  基础键码，报 `CSI 98;3u`，字节本身是对的——但 zsh 5.9 不支持 kitty 的
  `CSI u` 形式（实测它把 `ESC[98;3u` 原样回显成 `8;3u`），所以 kitty 模式下
  zsh 同样认不出。

对照：Zed 报的是 ∫，因为它把 option 当 Alt 语义处理，**不把 ∫ 当文本发送**。

### 现象 2 的链路

中文输入（鼠鬚管，`inline_preedit: true`）时拼音只有首字母可见。静态排查
（gpui macOS 事件层 → `set_marked_text` 透传 → `EntityInputHandler` 适配层 →
`TerminalView` 实现 → 绘制）逐环皆通，矛盾出在 zhimaterm 自己的 IME 查询
回答上：`text_for_range` 返回 `None`（没有可读文本）、`selected_text_range`
固定返回 `0..0`、`marked_text_range` 却返回 `0..n`（存在组合区）。输入法
更新组合串前要读组合区现状来计算替换范围，读到空就只能停在首字符。对照：
仓库自带的文本输入组件（`src/ui/input/core.rs`）有真实缓冲区，三个回答
自洽，中文输入正常。

Linux/Windows 平台没有现象 1 的问题：Linux 的 `key_char` 就是原始按键字符
（`gpui-pre-linux/src/linux/platform.rs:1520-1522`），不受 option 影响。

## 决策

### 1. option 修饰时按键文本取基础键码，不取系统合成字符

`key` 字段在所有平台都是「不含 option 修饰的按键标识」，是可靠的「基础键」来源。
option 修饰的文本键改用 `key` 派生基础键码，不再用 `key_char`。空格是唯一键名
（`"space"`）不等于其字符的文本键，派生与 `composed_char` 比较前先还原成 `" "`，
否则 ⌥+space 会把键名字面 `space` 发进 PTY：

- legacy：`ESC` + 基础字符（`ESC b`），这是 xterm 的 alt/meta 惯例，
  程序收到的是"带 meta 的 b"而不是一个 Unicode 符号。
- kitty：`CSI 98;3u`（键码 98 = 'b'，修饰参数 3 = alt），与协议一致。

这同时消掉了 `<ffffffff>`：zsh 收到 `ESC b`，识别为 meta+b，绑定正常。

### 2. 保留 macOS 原生特殊字符输入，作为可选开关

系统原生行为（option+b 输入 ∫）有真实用户——它能直接打数学符号。
因此加设置项 `option_as_alt`：

- 默认 **false**：保持 macOS 原生行为，option+字母输入特殊字符。
- true：option 当 Alt，基础键码 + alt 修饰。

与 kitty 的 `macos_option_as_alt`（默认 `no`）、Alacritty 的 `option_as_alt`
（默认 `false`）、Zed 的 Option-as-Alt（默认关）一致：三家主流默认都是
「option 产生特殊字符」，所以 zhimaterm 默认也关。

**关闭时怎么发字节**：仍然要发特殊字符本身（`∫` 的 UTF-8），**不加 ESC 前缀**。
当前的 `ESC` + `∫` 是两种语义混用，程序无从解码——这正是 `<ffffffff>` 的成因。
关闭时发纯 `∫`，终端和 shell 会把它当普通文本显示，与 macOS 原生一致。

### 3. 中文输入：补齐 IME 查询契约，组合串成为可读的虚拟缓冲区

拼音只显示首字母的根因是 `TerminalView` 的 IME 契约自相矛盾：`marked_text_range`
声称存在组合区（`0..n`），`text_for_range` 却返回 `None`（声称没有可读文本），
`selected_text_range` 永远答 `0..0`。输入法（鼠鬚管 inline_preedit）更新组合前
要读「组合区里现在是什么」来算替换范围，读到空就只能从首字符重来。

修复：把组合串当作**只含组合内容的虚拟缓冲区**——

- `text_for_range` 返回组合串的对应片段（UTF-16 偏移，macOS NSRange 口径）；
- `selected_text_range` 返回组合区内的选区（输入法用它标高亮段），随
  `replace_and_mark_text_in_range` 给出的 `new_selected_range` 更新；
- `replace_and_mark_text_in_range` 按 `replacement_range` 做 UTF-16 区间替换
  而不是整体覆盖（部分输入法按词分段替换）。

不采用「`accepts_text_input` 在 option 修饰时返回 false 来绕过 IME 优先分支」：
gpui 里该标志同时控制每帧的输入会话启停与 pending 输入超时，置 false 会把
中文输入整个关掉。

### 4. preedit 颜色不取光标格前景（第二层根因，真机诊断定位）

IME 契约修复后 shell 里正常，但 pi 等 TUI 里仍「只显示第一个字符」。运行时
诊断（IME 入口日志 + preedit 区铺色块）证明数据流与绘制逐环正常：输入法每键
发整串、`ime_composition` 正确更新、每帧都以新文本调用 `paint_preedit`。
真正原因是颜色：preedit 文字色取**光标格的前景色**，而 TUI 的光标块是反相
显示——该格前景恰与界面底色同系，preedit 画了也看不见；只有第一个字符压在
亮色光标块上才显出来（这正是「只见首字符」的假象）。

修复：`GridPainter` 增加 `preedit_foreground`，取主题默认前景
（`tokens.fg`），与格子内容解耦；render 组装时从主题 token 注入。

补充（2026-10，第三层）：组合文本（输入法 preedit）是**终端自己画的内容**，
不是应用画的内容。它要盖住应用画在那一格的东西——TUI 的反相假光标、输入条
底色、占位文字——底色与文字用**终端配色方案的默认色**（程序 OSC 10/11 改写
优先，否则主题色）。

这与 macOS 原生 IME 的做法一致：kitty 里的 marked text 是系统用终端默认色
画的（实测黑底 #000000 + 浅字 #eeeeee，而应用自己的输入条是 #1e1e1e），
盖住应用画在那一格的内容。之前 zhimaterm 把组合文本直接画在那一格上、用
界面的默认前景色，于是它和应用自己的文字混成一片；TUI 的反相假光标那格是
深底，浅色主题下第一个字符还会被吞掉——「第一个字符总是被遮住」。

配套：组合文本按字形簇逐个折算占格（`unicode-width` 的 wcwidth）、逐簇钉回
网格（宽字符占两格并居中，组合符号零宽、贴在基字符后面），组合区的底按占格
宽度铺，与网格对齐。

副作用（与 kitty 一致）：组合区盖住应用画在其下的文字——提交后应用重画，
未提交期间那一段是组合内容。

已知边界（`unverified`，人工验收）：`option_as_alt` 开启后，option+字母仍会
先进系统 IME（gpui 的 `is_ime_printable_key` 分支）。中文输入法对 option 组合
一般回退给应用（`doCommandBySelector` → KeyDown 路径 → 编码），若实测某输入法
直接把 ∫ 插成文本，再评估是否在 gpui 层面解决。

## 后果

- 可见变化：开启 `option_as_alt` 后，⌥+字母在 shell 里是 meta 键
  （`ESC b`/`ESC f`），zsh/bash/fish 的 `ESC b` 按词跳、`ESC d` 删词可用；
  vim/tmux 的 `M-字母` 绑定生效；kitty 协议程序收到 `CSI 98;3u`。
- 关闭时（默认）：⌥+字母输入 ∫ 这样的特殊字符，**且不再出现 `<ffffffff>`**——
  改发纯特殊字符字节，shell 按普通文本显示，与 macOS 原生一致。
- 中文输入：组合区可读后输入法能持续更新整串拼音；IME 渲染与提交路径不变。
- 已知边界：`composed_char` 判定（`key_char` ≠ 基础键）按数据驱动，跨平台
  一致，无平台分支；Linux/Windows 的 `key_char` 本来就是基础键码，开关对
  它们无行为影响；option 修饰的功能键（方向键、enter）本来就走按键通道，
  不受影响。2026-10-28 修订：设置开关只在 macOS 渲染（其余平台是死设置，
  不展示），文案随之去掉「（macOS）」后缀；`option_as_alt` 字段与持久化
  不因平台分叉。
- 测试：模型层 `composed_special_character_is_sent_as_plain_text_by_default`、
  `option_as_alt_reports_the_base_key_code_with_meta_prefix`、
  `kitty_mode_reports_the_base_key_code_even_with_composed_key_char`；
  设置层 round-trip 测试覆盖新字段。IME 契约与绘制效果进
  [人工验收清单](../context/manual-acceptance.md)。
