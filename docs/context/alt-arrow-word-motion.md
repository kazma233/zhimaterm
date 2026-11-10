# 调研：⌥+方向键的按词跳转——各终端机制与口径选择

状态：已定口径（2026-11，终端层翻译 `ESC b/f`，决策修订见
[ADR-031](../decisions/031-kitty-keyboard-and-modifier-encoding.md) 修订段）。
记录实测证据与各家实现路线，供以后处理「修饰键在 shell 里失效」类问题时参考。

## 现象

本地终端（裸 zsh 提示符）按 ⌥+←/→ 期望按词跳转，实际输出字面 "D"；
同一个键在 pi 等 TUI 里正常跳词。用户实测 kitty「直接进入的 shell 能跳」
更困惑——下面是逐层拆解后的完整事实。

## 编码层事实（三家终端相同）

方向键是 PC style function key，带修饰发 xterm 修饰参数形式：
⌥+← = `\e[1;3D`。TUI（pi 等）自己解析修饰 CSI，所以三家终端里 TUI 都正常。
**裸 shell 的差异全在 shell 侧拿到序列后有没有绑定**：

- zsh 默认键表只绑 `^[b`/`^[f`（meta 形态）→ backward/forward-word
  （实测 `bindkey -M emacs`）；`\e[1;3D` 无绑定，zle 把序列尾巴当字面插入
- bash readline 同样只认 meta 形态（M-b/M-f），修饰 CSI 插入 "1;3D"
- zsh **不会**自动绑定 terminfo 扩展能力（`zsh -f` 实测：xterm-256color
  定义了 kLFT3 也不绑）
- macOS `xterm-256color` terminfo 定义了 kLFT3/kRIT3，但没有消费者

## 各终端让「裸 shell 开箱即用」的机制（实测）

| 场景 | 跳词？ | 机制 |
|---|---|---|
| kitty 直接派生的 zsh | ✓ | **shell 集成注入绑定**：`kitty-integration` 脚本 `bindkey "^[[1;3D" backward-word`（仅在 undefined 时） |
| kitty 里嵌套 `zsh` | ✗ | 集成只注入直接派生的 shell；`cat -v` 显示发的仍是 `\e[1;3D`，无绑定 → 输出 D |
| Ghostty 1.4 任意 shell | ✓ | **终端 keybind 层翻译**：默认 `keybind = alt+arrow_left=esc:b` / `esc:f`，字节根本不进编码层 |
| iTerm2 | 预设后 ✓ | "Natural Text Editing" 预设 = ⌥+← 发送 ESC 序列 `b`（同形态） |
| ZhimaTerm（修复前） | ✗ | 标准编码 `\e[1;3D`，无绑定、无集成 |

两条路线的分野：

- **shell 集成绑定（kitty）**：只救终端直接派生的本地 shell——嵌套 shell、
  SSH 远端 shell 永远没有集成，照样坏。
- **终端层翻译（Ghostty/iTerm2）**：`ESC b/f` 是 zsh/bash/fish 全部默认
  绑定的形态，对嵌套 shell 与 SSH 远端同样生效；TUI 也认 alt+b（pi 正常）。

## 口径（ADR-031 修订）

ZhimaTerm 是 SSH 客户端，远端 shell 不可能有本地集成——走终端层翻译：

- **legacy 编码下**，纯 alt（无 ctrl/shift）的 ←/→ 发 `ESC b` / `ESC f`
- ctrl/shift 组合（含 ctrl+alt）维持修饰 CSI——meta 形态表达不了 ctrl
- kitty disambiguate 激活时**不翻译**——协商了键盘协议的程序（pi 等）
  要标准形式，它们本来就自己解析
- 翻译在编码层（`terminal::input::encode_key`），本地与 SSH 远端同时生效

## 可复用的诊断方法

- 「修饰键在 shell 里失效、TUI 里正常」→ 先 `cat -v` 看终端实际发出的
  字节，再 `bindkey '<序列>'` 看 shell 侧绑定：两个终端的体验差异几乎
  总是这两层之一造成的。
- 对比「主流终端行为」必须实测版本本体：Ghostty 1.4 的开箱即用来自默认
  keybind 翻译（`ghostty +list-keybinds` 可查），kitty 来自 shell 集成
  绑定——查源码编码层只会得出「三家相同」的正确但无用结论。
