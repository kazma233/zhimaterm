# ADR-050：终端核心后端抽象与 termwiz 后端

状态：已接受（2026-11）

## 背景

终端核心（VT 解析 + 网格 + 滚动历史 + 查询应答）此前由 `alacritty_terminal`
独家承担，`TerminalModel` 直接持有它的 `Term` 与 `ansi::Processor`，选区与搜索
也直接用上游的 `Selection`/`RegexSearch`。要支持「按设置切换终端核心后端」，
需要先回答两个问题：

1. **切换粒度**。已运行会话的终端状态（模式、备用屏、滚区、kitty flags、
   OSC 改写表、同步输出缓冲、选区）无法无损迁移到另一个核心；重放原始字节
   也不是等价迁移——会二次触发 OSC 52、铃声、标题等副作用。可切换的只有
   「新会话」。
2. **一致性由谁保证**。如果两个后端各自实现快照、选区、搜索，「切换」就
   等于换了一个行为不同的终端。一致性必须由结构保证：语义只写一份。

## 决策

### 1. 分层：语义在共享层，字节 → 网格在后端

```
views/ ──── TerminalSnapshot ──── TerminalModel（共享语义层）
                                        │  TerminalCore (trait)
                            ┌───────────┴────────────┐
                     backend/alacritty          backend/termwiz
                     Term + vte::ansi           Parser + 自研 Screen
```

- **后端契约**（`terminal/backend/mod.rs`）：`TerminalCore`（advance/resize/
  滚动/模式/光标/调色板/查询请求/同步输出）与 `GridView`（行读取、折行标记、
  行长、制表位）。中性类型：`GridCell`、`RowBuffer`、`CellColor`、`ColorPalette`、
  `TermModes`、`CursorState`、`GridPoint`、`Rgb`、`WindowSize`。上游类型
  （`Term`、`Cell`、`Flags`、`TermMode`）止步于 `backend/alacritty.rs`。
- **共享层**（`terminal/mod.rs` + `selection.rs` + `search.rs` + `colors.rs` +
  `transcript.rs`）：画面快照、选区（Simple/Word/Line/Block 与复制文本）、
  搜索（逐行正则 + 智能大小写）、颜色解析、输入编码、转录与回滚恢复，只有
  一份实现。选区与搜索不再用上游的，语义按上游行为移植后固化为本仓口径
  （ADR-015 的「匹配不跨行」等边界不变）。
- `TermRequest` 完全中性：颜色查询用 `ColorSlot { Indexed, Foreground,
  Background, Cursor }`，不携带任何后端的槽位编号。

### 2. 切换粒度：设置项，新会话生效

- `Settings.terminal_backend: Option<String>` 存名字（与主题同一纪律）：
  名字不存在或当前构建未编译该后端时回落默认，不报错。
- `SessionConfig.backend` 随会话构造定型；`TerminalView` 记住本格后端，
  **重连沿用原格后端**，不读当前设置。
- 本格用的哪个后端对用户可见：首次画面到达时画面中央闪现后端名约 1 秒
  （`terminal_view.rs`）。常驻标识挤占终端画面，闪现只在开格最需要
  确认的时刻出现。
- 运行中的会话不接收后端切换；不提供「重启会话以换后端」（会丢 shell 状态，
  收益不抵代价）。
- 默认后端是 alacritty（新会话未配置时）。两个后端都是普通依赖、始终编译
  进包，不做 feature 门控——「可选但默认全开」的两面性只会留下一条永远不
  走的路径。

### 3. 一致性：契约测试 + 差分测试

- **契约测试**（`terminal/contract.rs`）：行为场景（打印、属性、宽字符、
  选区、搜索、备用屏、鼠标、设备查询、kitty、同步输出……）对全部后端各跑
  一遍，红一个就是语义分叉。现有 26 个场景；新后端接入时加进 `backends()`。
- **差分测试**：resize 场景在两个后端各跑一遍、逐检查点对比整张
  `TerminalSnapshot`，以 alacritty 为参照。8 个场景覆盖缩宽/增宽、增/减
  高度 × 光标位置、宽字符、奇数宽度跨列、光标在正文中间。
- **随机脚本 fuzz**（2026-11-06 补）：termwiz 后端的确定性 fuzz 骨架提升
  到模型层，全部后端跑同一份随机操作流（输出、光标、擦除、滚区、插删、
  备用屏、同步输出冻结、resize、回看滚动），断言共享层不变量（快照自洽、
  稳定、滚动界内）；回放语料（`terminal/replay.rs` 的确定性脚本与外部
  录制）同样对全部后端回放。首个版本即抓出 termwiz 后端两处行宽腐化
  （打印落宽字符尾格占位未让位、插删字符按裸格平移劈开占位配对），均已
  修复并以回归测试锁定。termwiz 自家的 core 级 fuzz 保留（内部网格状态
  断言是其专属深度）。

### 4. 已知有意分歧（不照抄上游）

| 项 | 上游（alacritty） | 本仓 |
|---|---|---|
| SGR 21 | 当 CancelBold（Linux console 口径） | termwiz 后端当双下划线（xterm/kitty/ECMA-48 口径）；共享 `UnderlineKind` 文档注明归后端决定 |
| 跨列占位后增宽 | 会丢内容（`LEADING_WIDE_CHAR_SPACER` 的边缘缺陷） | termwiz 后端保证内容完整（有专门断言 `grow_after_straddle_keeps_all_content`） |
| 制表位 | 上游不外露其制表位表，alacritty 适配层按默认 8 列近似 | termwiz 后端跟踪真实 HTS/TSD 制表位；ghostty 后端同样不外露，按 8 列近似（ADR-051） |
| emoji 字形簇宽度 | 逐码点定宽：`⚠️` 占 1 格、`👍🏽` 占 4 格，应用按簇口径补的空格对不上（表格边框错位） | termwiz 后端按簇口径回写：VS16 加宽 / VS15 收窄 / 修饰符并入 / ZWJ 序列整簇 2 格（2026-11-04 补，见下） |
| OSC 颜色应答结尾符 | 按查询结尾镜像（BEL 查询回 BEL，vte 的 `bell_terminated`） | 共享格式器统一固定 ST（kitty/ghostty 口径，双写十六进制不变）；BEL 结尾查询的应答随统一变为 ST（ADR-051 §2.1 修订，应答格式归共享层单点） |
| 选区止于行末让位格 | 取上一行首字拼进复制内容（`grid[line - 1]`，与其自身注释「带出该宽字符」矛盾的错行缺陷） | 共享层经 `wrapped_wide_char` 取**下一行首列**的宽字符（含组合字符）；三后端共用同一实现（2026-11-06） |

上游欠账（跟进上游修复后移除近似）：alacritty_terminal 未暴露制表位表与
SGR 21 的可选语义；`Term::input` 逐码点定宽且无宽度覆盖入口，簇口径无法
在适配层补救，只能等上游跟进后移除本条分歧。

### 5. emoji 字形簇宽度（termwiz 后端，2026-11-04 补）

程序（claude 一类表格生成方）按**字形簇**算宽：VS16 序列（`⚠️`）、肤色
修饰符（`👍🏽`）、ZWJ 连接（`👨‍👩‍👧‍👦`）、键帽（`1️⃣`）都占 2 格；
kitty/Ghostty/WezTerm 同口径。alacritty 的逐码点口径对不上，表格边框错位
（诊断记录：`⚠️` 网格 1 格 vs 应用 2 格，整行差 1 列）。

termwiz 后端的屏幕模型在本仓，`print` 落格后按簇规则**回写**光标前簇：

- 判定内核（Ghostty `graphemeWidthEffect` 口径）：VS16/VS15 只对
  emoji-variation-sequences 的合法基字符生效（改宽度），非法基字符整个
  丢弃不留痕；修饰符只并入 emoji 基；ZWJ 只连 emoji（GB11，`x‍b` 照常
  断开）；其余零宽字符挂簇尾。基字符表内嵌自
  emoji-variation-sequences.txt（Unicode 16.0/17.0 内容一致）。
- **回写不缓冲**：末字符对快照立即可见（纯缓冲方案会让流停在簇中间时
  末字符不可见，被契约测试否决）；字节块劈在簇中间不影响结果。
- 已知残留差异：行末放不下加宽时保持 1 格只挂选择符——Ghostty 会整簇
  折行重排，回收已落格内容需要缓冲，不值得；组合符挂宽字符基格而非尾格
  占位（顺带修复了旧孤儿路径挂错格的问题）。
- alacritty 后端维持逐码点宽度；契约测试只跑两后端共同语义，簇宽度的
  断言在 termwiz 专属测试（`backend::termwiz::cluster_width`）与单后端
  快照测试（`termwiz_cluster_width_reaches_the_snapshot`），模糊测试样本
  加入 emoji 序列覆盖回写路径。

## 后果

- 视图层与 worker 的接口零变化：`TerminalSnapshot` 契约不变，worker 只在
  构造模型时多传一个后端参数。
- 换后端不改变终端对外的行为——由契约测试在结构上保证，而不是靠两份实现
  人工对齐。
- 后端专属能力（如图像协议）若要进入画面，必须是 `TerminalSnapshot` 契约的
  显式扩展，不允许视图层出现后端分支。
- 默认构建即带双后端，`cargo test` 一次跑完契约与差分场景，不存在「某套
  构建少测一个后端」的盲区。
- termwiz 后端的完整性与契约测试等价，但真实环境（多种 TUI、IME、SSH）的
  长尾兼容性仍以 alacritty 后端的积累为准——这也是默认后端保持 alacritty
  的原因。

## 验证

- `cargo test`：420 项通过（契约与差分场景双后端跑）。
- `cargo clippy --workspace --all-targets` 0 warning。
- resize 差分场景逐帧快照相等（含光标格、颜色、属性、历史与视口分布）。
- termwiz 后端的确定性模糊测试（64 seeds × 1500 步随机输出 + 随机 resize +
  备用屏进出 + 同步输出）断言不变量：会话线程上任何 panic 都会带走全部
  会话，后端不允许越界。
