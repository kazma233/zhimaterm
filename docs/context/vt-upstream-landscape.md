# 三个 VT 核心的上游动态与社区口径（2026-11 调研）

2026-11-20/21 讨论沉淀：复盘 ADR-051 引入 ghostty 的动机 → 评估
alacritty_terminal / termwiz / libghostty-vt 的社区口碑与上游走向 → 得出
[ADR-060](../decisions/060-as-is-vt-backend-convergence.md) 的收敛决策。
事实为 2026-11 快照；时效性结论引用前复核。

## 1. 三核心社区画像

| | alacritty_terminal | termwiz | libghostty-vt |
|---|---|---|---|
| 社区定位 | 「拆出纯仿真核心」先行者（[#823](https://github.com/alacritty/alacritty/issues/823)，2017） | 最完整的 Rust 终端模型，为 WezTerm 自用而生 | 新生代，Unicode 正确性冠军 |
| 主要赞誉 | 架构分层好、真实环境积累最厚 | 簇口径完整、功能面最全 | 零依赖、簇口径原生正确、生态起速快 |
| 主要批评 | 逐码点定宽是长期缺口；API 随主程序需求变 | 独立使用文档薄、无稳定性承诺、用户面窄 | 项目年轻；性能声明被质疑；早期仅承诺 macOS |

kitty 附注：Kovid Goyal 从零自研（C + Python，含 2024 年 SIMD 重写的
自有解析器，[#7005](https://github.com/kovidgoyal/kitty/issues/7005)），
Unicode 与 ghostty 并列冠军（[Jeff Quast 2025 评测](https://www.jeffquast.com/post/state-of-terminal-emulation-2025)），
但 GPL-3.0 且核心从未库化——对本仓不可嵌入，能复用的是其协议发明
（ADR-031 的 kitty 键盘协议即是）。

## 2. alacritty：簇宽度为什么修不了（[#3975](https://github.com/alacritty/alacritty/issues/3975)，2020-07 开）

三层立场（均有维护者原话支撑）：

1. **wcwidth 契约立场**（kchibisov）：应用按逐码点测量，终端按簇合并会让
   两边宽度脱钩，按 wcwidth 算宽的应用反而错位；终端与客户端还要对齐
   Unicode 版本，「双向协商永远是坏主意」。
2. **正确修法太贵**（chrisduerr）：像样的簇支持要 HarfBuzz shaping，与
   速度/极简依赖定位冲突；可接受的只有「保宽合并字形」的 hack——
   issue 挂着就是留给它的，五年没人写。
3. **维护者无需求**：原话「I don't need it」「I have little interest in
   this feature」；P-low，52 个 👍 推不动。

近一年（2025-11~2026-11）只发 0.17.0（2026-04：滚轮绑定、TOML 1.1、
崩溃修复为主），0.18-dev 七个月全是修复条目；核心语义、簇宽度零变化
（[releases](https://github.com/alacritty/alacritty/releases)）。社区修法
是外置 crate（helix 维护者的
[grapheme-width-rs](https://github.com/pascalkuthe/grapheme-width-rs)），
上游未集成。

## 3. wezterm [#4223](https://github.com/wezterm/wezterm/issues/4223)：簇口径的正反方碰撞

`👩‍❤️‍👨` 例子：传统 wcswidth 求和 5~6 格 vs wezterm 整簇 2 格
（`termwiz` 的 `.min(2)` 封顶）——按传统算宽的应用（Helix 一类）在
wezterm 上光标错乱（「随机高亮、消失与幽灵字符」）。这是 alacritty
#3975 的镜像：那边是「终端守旧、应用革新」错位，这边是「终端革新、
应用守旧」错位，同一条裂缝的两面。

各方立场：

- wez：iTerm2 同款；拒绝回滚（「undo grapheme clustering is a PITA」）；
- kovidgoyal（kitty 作者）：反而站 wezterm（「2 格才对，错的是不做的
  终端」），主张全生态收敛到一个标准 wcswidth，反对应用做终端检测
  workaround；
- 收场：不改宽度，实现 **DECRQM 2027** 查询（[#4320](https://github.com/wezterm/wezterm/issues/4320)），
  应答「永久开启」，让应用可检测。

mode 2027 谱系：Contour（christianparpart）立 spec（terminal-unicode-core）
→ wezterm/foot/contour 可查 → **ghostty 默认开启**（ADR-051 买到的口径）
→ Windows Terminal 正确处理。

## 4. libghostty-vt 1.0（2026-11-06，[公告](https://mitchellh.com/writing/libghostty-1-0)）

- **C ABI 语义化稳定**：enum 键控 get/set（加值不加符号）、新头文件链
  旧库返回 `GHOSTTY_UNSUPPORTED`、struct 带尺寸可追加字段；上游用
  libabigail + 类型金 manifest 在 CI 强制——本仓预生成绑定提交的前提
  被制度化。
- **采用面**：JetBrains 全系 IDE 终端（约 1250 万活跃用户）、Neovim
  替换 libvterm 的 PR（此前等的就是 tagged release）、KDE 试验性采用、
  Vercel Turborepo、Antithesis 模糊测试；社区绑定覆盖十门语言，
  awesome-libghostty 收录百余项目。
- **可移植性**：MIT、运行时仅依赖 libc，CI 实测含 Linux glibc/musl、
  macOS 13+、iOS、FreeBSD、**Windows**、Android、wasm32——缓解
  ADR-051「官方仅承诺 macOS runtime」的平台风险记录（CI 实测 ≠ 日常
  使用验证）。
- 本仓已 pin 1.0.0（commit `62de9e9`，2026-11-09，发布后第三天，与
  Neovim 一批「等 tagged release」的嵌入方同节奏）。

## 5. 对本仓的映射（结论）

1. 「等 alacritty 上游」不可期——立场性不修 + 维护节奏（一年一个 minor，
   核心零变化）双重证明；ADR-050 §4 的上游欠账表述应按长期不发生规划。
2. 簇正确性的终态承载只能是 ghostty；termwiz 簇回写是被两天后出现的
   ghostty 后端接走一半存在理由的过渡产物（2026-11-04 回写 →
   2026-11-06 ghostty）。
3. 生态分裂（wcwidth vs 簇）是常态，用「双后端各原样 + 分歧表记账」
   对冲，不用适配层抹平——见 ADR-060。

## 6. 来源

- alacritty：[#3975](https://github.com/alacritty/alacritty/issues/3975)、
  [#823](https://github.com/alacritty/alacritty/issues/823)、
  [releases](https://github.com/alacritty/alacritty/releases)
- wezterm：[#4223](https://github.com/wezterm/wezterm/issues/4223)、
  [#4320](https://github.com/wezterm/wezterm/issues/4320)
- Mitchell Hashimoto：[Grapheme Clusters and Terminal Emulators](https://mitchellh.com/writing/grapheme-clusters-in-terminals)、
  [Libghostty Is Coming](https://mitchellh.com/writing/libghostty-is-coming)、
  [Libghostty 1.0](https://mitchellh.com/writing/libghostty-1-0)
- Jeff Quast：[State of Terminal Emulators in 2025](https://www.jeffquast.com/post/state-of-terminal-emulation-2025)
- kitty：[仓库](https://github.com/kovidgoyal/kitty)、[#7005](https://github.com/kovidgoyal/kitty/issues/7005)
- [grapheme-width-rs](https://github.com/pascalkuthe/grapheme-width-rs)
