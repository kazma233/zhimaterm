# ADR-059: 连字开关与手输字体特性

状态:已接受(2026-11-17)

## 背景

等宽编程字体的连字(`->`、`!=` 合成一个字形)与 OpenType 特性(`ss01`、
`zero` 等)无处可配。整形完全发生在应用侧 gpui:libghostty-vt 是 VT 状态机,
不参与字形整形;gpui 的每根 `Font` 都带 `FontFeatures`(平台层 CoreText /
DirectWrite 真实消费),`FontFeatures::disable_ligatures()` 就是现成的
`calt=0`——能力齐备,应用侧只是从未设置过(构造字体时恒为默认空,即连字
始终开启)。ADR-008 已定性段内连字「照常成形但钉在首格、宽度可能压格线」
是逐格钉位模型的固有取舍。

## 决策

### 两个设置项,只作用终端网格字体

- `font_ligatures: Option<bool>`,None = 开。默认开 = 保持既有渲染行为,
  不回归;关 = 网格整形带 `calt=0`。
- `font_features: Option<String>`,手输特性串原样保存:逗号分隔的
  「tag value」对(如 `ss01 1, zero 1`),tag 为 4 位字母数字(gpui 同口径),
  value 为非负整数。
- 生效面只有终端网格(`font::FontConfig.features` → `terminal_grid_font`):
  网格绘制与格尺寸测量同用一份 config(「两者必须一起换」的既有纪律)。
  表单输入、对话框、SFTP/探针面板与下拉预览继续用无特性的
  `terminal_font(family)`——界面文字不需要连字,也不该被手输特性波及。
- 「应用」时经既有 `set_font` 扇出即时生效;特性不改变等宽度量
  (calt/ss01 类特性不改 advance),`measure` 仍照常走一遍保证同源。

### calt 的唯一所有者是连字开关

- 表单校验拒绝手输 `calt`:两个入口写同一个 tag 必然打架,连字语义只允许
  一个写入方。
- 手改 settings.json 写入 `calt` 视为权力用户的显式直写,但**不另开一条
  写入通道**:读入时把 `calt` 折进连字开关(`calt=0` = 关连字,其余 = 开),
  特性列表里不再留 `calt`。折进单点在 `settings::split_ligature_tag`。
  折进后 `calt=1` 等价于「连字开」——字体默认本就开,不额外产生特性项。
- 折进同时解决了表单的「死结」:文件里带 `calt` 时,表单预填把 `calt`
  摘掉、只把其余特性放进输入框——否则预填串过不了严格校验,用户永远
  点不了「应用」。表单预填因此永远是校验能过的形态。
- 合流单点在 `settings::merge_font_features`:开关两个状态都**显式**下发
  `calt`(开 = 1、关 = 0),不留「不写、交给字体默认」的中间态——实测
  (2026-11-17,Fira Code/macOS/CoreText,网格整形探针)不写 `calt` 时
  `->` 不成形:CoreText 的默认特性集不含 contextual alternates,「连字开
  」若不下发 `calt=1` 就等于关。同一 tag 全程只写一遍。

### 校验与手改文件的纪律

- 表单:`validate_font_features` 严格校验,坏 tag/坏取值/缺 value/重复 tag
  各有明确文案,错误落表单底部(设置表单的既有口径);空串 = 不配置。
- 手改文件:`lenient_font_features` 跳坏项留好项——文件没有错误面可贴,
  静默丢弃一个坏项比让整个特性串失效(更悄无声息)可取。

## 已知限制（2026-11-17 实测：macOS 连字当前不成形）

设置项在 macOS 上**当前没有可见的连字效果**，这是整形栈的平台事实，
不是设置实现的缺陷。取证链（三层独立验证，字形数为判据——连字成形时
`->` 合成一个字形，逐字符则是 2 个）：

1. **生产渲染路径**：网格绘制探针打印整形字形数。Fira Code 6.002、
   `dir->test` 31 字符 → 31 字形（逐字符），`FontFeatures { calt: 1 }`
   已确认下发到 gpui 的 `Font`。
2. **独立 CoreText 实验**（Swift + CTLine，绕开 gpui）：default /
   `calt=1` / `calt=0` 三种 feature settings 下 `->`、`x->y`、`=> != ===
   <- <=` 全部逐字符——CoreText 的通用整形（CTLine）对 calt 完全不应用,
   显式 feature settings 也无效。
3. **字体侧排除**：fontTools 解析 FiraCode-Regular.ttf 6.002，GSUB 含
   calt（101 个 lookup）、无 liga——连字规则在字体里存在无疑。

结论：gpui-pre 0.3.12 的 macOS 整形坐在这条 CTLine 路径上，连字从不成形
（ADR-008 的「段内连字照常成形」在该栈上不成立，todo.md 的「连字跨格成形
未实测」实测关闭）。因此连字开关在 macOS 两态同显（都逐字符）；手输特性
的可见性存疑但保留配置面。上游若换整形栈（或 gpui 升级带 OpenType 整形）
后本节自动失效，届时重跑第 1 层探针即可验证。Windows（DirectWrite
IDWriteTypography）与 Linux 待实机验证，见聚合待办。

## 后果

- `base::font` 的 `FontConfig` 增加 `features` 字段;网格渲染与测量改走
  `terminal_grid_font(config)`,其余 `terminal_font(family)` 调用点不动。
- 设置表单增加连字开关行(勾选框机制与 ⌥ 开关同款,解除 macOS 门控改
  全平台)与特性输入框;`SettingsValues` 增至 12 个编辑值。
- 对三个终端后端一视同仁:整形不在后端,后端无关。
- 连字的「钉首格、可能压格线」取舍不变(ADR-008);关连字 = 回到逐字符
  显示,钉位宽度问题随之消失。
