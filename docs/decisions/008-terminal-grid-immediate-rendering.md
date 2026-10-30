# ADR-008：终端网格的立即模式渲染与字形回钉

状态：已接受

## 背景

ADR-003 确立了 worker 持有终端状态、UI 只消费快照的边界，但视图侧最初用 div 逐行装配 run：每段 run 一个定宽 div，文本在段内流式排版。该实现暴露两个问题：

1. **字形漂移**。光标/选区格样式唯一、必然自成一 run，光标每移动一格，整行 run 的切分点就变化一次。段内字形 x = 段起点 + 真实步进累积，只要任一字符的真实步进 ≠ 格宽（回退字体符号、powerline 字形、emoji），误差就在段内累积；切分点一动，累积重新归零，光标右侧文字经 gpui 的 1/4 像素亚像素量化后表现为肉眼可见的左右漂移。
2. **布局树规模**。gpui 的 div/taffy 是为少量界面元素设计的保留模式布局。消除漂移的直接做法是逐格展开元素，但满屏 ~3000 节点每帧重建重排，实测卡顿不可用。

## 决策

网格渲染改为**立即模式绘制 + 字形级回钉**，全部走 gpui 公开 API：

- 终端区域只挂一个绝对定位 `canvas`，paint 闭包内完成整帧：每段 run 一个
  `paint_quad` 背景；文本经 `WindowTextSystem::shape_line_by_hash` 整段整形一次
  （内容哈希做缓存键，命中时免物化文本、零分配），
  再遍历 `ShapedGlyph` 把每个字形钉回「列 × 格宽 + 簇内相对偏移」，
  经 `paint_glyph`/`paint_emoji` 画入场景。网格不进 taffy 布局树。
- 字形 x 只由列号决定，与 run 切分点彻底解耦——漂移的根因消除。
  Windows Terminal AtlasEngine 的 grid-fitting 同此思路；Alacritty 则以不支持连字为代价逐格锚定。
- 下划线整段一条 `paint_underline`，y 用公开的 `underline_y_offset`
  （与 gpui 内部同一公式，位置一致）；钉位路径不经过装饰逻辑，避免双重绘制。
- 快照存 `Arc<TerminalSnapshot>`：绘制闭包每帧克隆一次句柄（一次原子操作），
  画面数据不借住 `&mut App`，也不拷贝整帧。
- 回退路径：字形簇数与格数对不上时（`cell_start_bytes` 返回 None）退回整段
  流式绘制保持旧行为——宽字符/emoji 整簇占两格，走的就是这条路径（span 内居中），
  并非只有异常网格数据；单格绘制失败记日志不中断整帧
  （与 gpui 内部 `log_err` 模式一致）。
- dev profile 对依赖开优化：`[profile.dev.package."*"] opt-level = 2`（见排障记录）。

### 被否的中间方案

**逐格整形**（每格一次 `shape_line` + `ShapedLine::paint`）：正确，但每帧
~2880 次缓存查找与 `paint_layer` 图层操作，debug 下明显掉帧。整段整形 +
手工回钉把整形次数降回 O(run 数)，字形 quad 数不变（这已是下限）。

**逐格 div**：~3000 个布局节点，布局复杂度 O(格数)，不可用。div/taffy 只用于
外壳框架部件，网格这种高频满屏内容走立即模式。

## 后果

- `view::render_row`/`render_run`（返回 div）删除，改为 `GridPainter`；
  `views/terminal_view.rs` 网格区域只装配 canvas。`grid_size`/`cell_at`（布局与命中）不变。
- 依赖新增 `unicode-segmentation`（按字形簇切格；gpui 依赖树里本就有，无新增编译单元）。
- 段内连字（如 `->`）照常成形但钉在首格，宽度可能略压格线——逐格钉位模型的
  固有取舍。需要更好连字支持时可升级为 AtlasEngine 式带连字宽度的回钉，
  模型层（run 结构）不用动。

## 性能排障记录（2026-10 实测）

症状：debug 构建下 cat 大文件满屏刷明显卡顿、输入延迟；release 正常。曾怀疑
网格重写引入回归，探针数据排除了这一点：

| 场景 | debug 网格耗时 | release 网格耗时 |
|---|---|---|
| 稳态（打字/移动光标） | 0.2–1.7 ms/帧 | 0.02 ms/帧 |
| cat 满屏刷 | ~100 ms/帧 | 4–6 ms/帧 |

结论：

- 稳态缓存命中路径极便宜（debug 也不到 2ms），重写无回归。
- cat 卡顿 = 每帧全部 run 内容更新 → `shape_line` 全部缓存未命中 → 全屏
  DirectWrite 整形。debug/release 差 ~20 倍，正是 gpui-pre 的 Rust 包装层
  （UTF-16 转换、COM 调用、字形回调解析）在 debug 下未优化；DirectWrite
  本身是系统 DLL，不吃 debug 税。
- 旧 div 版本在 cat 场景同样要付这笔整形费（同一条整形代码路径）。「旧版不卡」
  的印象来自静止画面场景：旧版靠 GPUI 元素状态缓存跳过重测
  （`elements/text.rs` 的 `TextLayout` 缓存），新版靠 shape_line 内容缓存命中，
  稳态两者都便宜。
- 修复：`[profile.dev.package."*"] opt-level = 2`——应用代码保持可调试、
  依赖全优化（Rust GUI 项目通行做法）。复测确认 cat 场景卡顿消除。
  排查方法：在网格绘制 canvas 闭包内计时、每秒汇总打印一次（逐帧打印会因
  Windows 控制台同步写出的毫秒级开销污染测量）；复测确认后探针已从代码中移除。

## 验证

- `cargo test --bin zhimaterm view`：布局、命中测试、簇切分测试全过。
- 字形漂移在逐格钉位后未再复现（用户实测反馈仅剩性能问题）。
- 排障数据见上表；opt-level=2 后经用户复测确认 debug 卡顿消除，探针随之移除。

## 相关

- [ADR-003](003-terminal-rendering-and-input.md)：终端渲染与输入边界。
  worker/快照契约未变，本文只改 UI 内部的网格绘制方式。
