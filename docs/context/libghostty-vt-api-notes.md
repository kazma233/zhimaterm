# libghostty-vt C API 机制笔记

> **先读这段**：本文是 2026-11 把 ghostty 后端从 crates.io 封装切到仓内薄绑定
> （ADR-051）时的一次性探索记录，**不是行为契约**——行为口径以 ADR-051 与契约
> 测试为准。libghostty-vt 自 1.0（vt-1.0.0 tag）起承诺 C ABI 稳定，但小版本仍
> 可能新增选项/枚举值；**扩充能力前必须二次确认**：
> 对照当前 pin 的官方头文件（`vt.h` 与 `vt/` 子头，见 §1）与上游封装实现（§10），
> 并用契约测试验证。标 ⚠️ 的条目只读过文档注释、未实测。

## 1. 权威源在哪

- 官方 C API 头文件：源码树 `include/ghostty/vt.h` + `include/ghostty/vt/*.h`
  （terminal/screen/render 等约 30 个子头，函数文档注释很完整）。本机获取路径
  三条（同 [绑定 runbook](../runbooks/ghostty-bindings.md)）：`GHOSTTY_INCLUDE_DIR`
  / `GHOSTTY_SOURCE_DIR/include` / `target/*/build/ghostty-vt-sys-*/out/src/include`。
- ⚠️ 源码树里另有 `include/ghostty.h`，那是 Ghostty 内部 embedder API
  （自述 libghostty-internal，仅 macOS app 消费），**不是本仓用的**。
- 本仓对 C API 的全部触面 = `ffi/ghostty-vt-sys/src/bin/gen_bindings.rs` 的
  allowlist（27 函数）+ `crates/terminal/src/backend/ghostty/vt.rs` 的封装。

## 2. tagged get/get 协议（扩充时最先撞上的一条）

`ghostty_terminal_get/set` 是通用读写入口：`(handle, tag, void* 出/入参)`；
render_state / render_state_row / render_state_row_cells / cell / row 各有一对。
**载荷类型不出现在任何函数签名里**（全是 `void*`）——bindgen 靠签名传递不到
它们，`gen_bindings.rs` 的 TYPES 必须显式列；漏列的表现为 bindings 里找不到类型。
指针语义两态：普通选项传「指向 Rust 存储的指针」；USERDATA 与回调选项传
「指针值本身」（vt.rs 的 `set` vs `set_ptr`）。

本仓用到的 tag → 载荷对应：

| 入口 | tag 枚举 | 载荷类型（用到过的） |
|---|---|---|
| terminal_get | TerminalData | 各标量（u16/u32/bool/usize）、`ffi::String`（TITLE）、`ColorRgb`（Option 走 NO_VALUE）、`TerminalScrollbar`、`TerminalModeConfig`（模式查询）、`[ColorRgb; 256]`（调色板）、u8（kitty flags） |
| terminal_set | TerminalOption | bool、usize、`ColorRgb`、`[ColorRgb; 256]`、`TerminalModeConfig`（模式设置）、回调函数指针、USERDATA |
| render_state_get | RenderStateData | sized `RenderStateCursor`（光标全量）、迭代器句柄（ROW_ITERATOR 的出参就是句柄指针） |
| render_state_row_get | RenderStateRowData | `ffi::Row`（u64）、迭代器句柄（CELLS） |
| render_state_row_cells_get | RenderStateRowCellsData | `ffi::Cell`（u64）、`Style`（1.0 起固定 16 字节布局）、`GRAPHEMES_UTF8` 走 Buffer（§4） |
| cell_get / row_get | CellData / RowData | bool（HAS_TEXT 等）、`CellWide::Type`（WIDE）、bool（WRAP） |

## 3. sized 结构协议

- 一批结构**首字段 `size: usize`，必须写 `sizeof(Self)`**：C 侧以此做版本协商，
  只读调用方已知的字段。用到：`RenderStateCursor`、`ClipboardWriteReply`。
  初始化用 `ghostty_vt_sys::sized!` 宏。
- 1.0 起 `GridRef`（node+x+y）与 `Style`（固定 16 字节）改为**冻结布局**，
  无 size 字段，`default()` 初始化即可（头文件明说不会在未来版本改动）。
- 不是所有结构都 sized：`DeviceAttributes`、`TerminalScrollbar`、
  `TerminalModeConfig`、`SizeReportSize`、`ColorRgb` 没有 size 字段。
- 出参读取模式：`MaybeUninit::zeroed()` + 成功返回后 `assume_init()`。

## 4. 两段式缓冲与结果码

- `Buffer { ptr, cap, len }`：SUCCESS 时 `len` = 写入数；OUT_OF_SPACE 时
  `len` = 所需容量 → 扩容重试。`GRAPHEMES_UTF8` 走这个。
- grid_ref 路径的 `graphemes`/`hyperlink_uri` 是裸数组版：`buf + len + &mut len`，
  所需容量经错误带回。
- `Result` 码族：SUCCESS / **NO_VALUE（→ Option）** / OUT_OF_SPACE /
  OUT_OF_MEMORY / INVALID_VALUE / IO_ERROR / LIMIT_EXCEEDED / REJECTED。

## 5. 回调机制

- **单一 USERDATA 指针全回调共享**；回调在 `vt_write` 期间同步触发，不能阻塞、
  不能在回调里写终端或取事件。别名纪律：C 只在 vt_write 期间触碰 userdata，
  而 vt_write 拿 `&mut Terminal`——与安全侧访问天然互斥（vt.rs 的 Shared）。
- 已装 10 个（vt.rs `install_callbacks`）：WRITE_PTY、BELL、TITLE_CHANGED、
  ENQUIRY、XTVERSION、SIZE、COLOR_SCHEME、DEVICE_ATTRIBUTES、CLIPBOARD_WRITE、
  RENDER_HOLD。
- 故意不装 3 个：PWD_CHANGED、DESKTOP_NOTIFICATION、PROGRESS_REPORT——适配层
  不消费这些事件；vt.h 语义：传 NULL = 关闭该效果。
- ⚠️ 从未装过的 6 个：CLIPBOARD_READ、PROGRAM_STATUS、UNKNOWN_SEQUENCE、
  SEMANTIC_PROMPT、RESET、RESIZE_WINDOW——只读过 vt.h 的选项表（terminal.h
  「Effects are callbacks」一节），行为未实测；扩充前先读对应文档注释。
- 应答型回调三形态：返回 bool（SIZE / COLOR_SCHEME / DEVICE_ATTRIBUTES，
  true = 有值）；返回 `ffi::String`（ENQUIRY / XTVERSION，null ptr = 无应答）；
  CLIPBOARD_WRITE **必须在回调返回前调 reply_fn**（本仓固定 SUCCESS +
  remember=false，授权策略若要细化看 ClipboardWrite 的 granted/can_remember 字段）。
- **SIZE 应答协议**：核心不记几何。`resize(cols, rows, w, h)` 时把几何写进
  userdata，SIZE 回调返回它——CSI 14/16/18t 的自答依赖这条链；漏了它，尺寸
  查询在首次 resize 之后也答不出（首 resize 前本来就不答）。

## 6. 渲染快照、迭代器与 grid_ref

- `render_state_update(terminal)` 批量快照**视口**并消耗核心脏状态；数据只在
  下次 update 前有效。行/格迭代器是独立句柄，「装到快照/行」本身就是 tagged
  get：`render_state_get(ROW_ITERATOR, &mut 句柄)`、`row_get(CELLS, &mut 句柄)`。
- 格迭代器 `select(x)` 定位列后 `raw_cell()/style()/graphemes_utf8()` 读该格。
- `grid_ref` 是低频慢路径（vt.h 明说不能当渲染内核）：固定布局出参，**下次终端
  更新即失效**（即取即用）。Point 坐标三态：Active（活动区，与回看偏移无关）、
  Viewport（视口）、History（历史区，y 从滚动区顶部数起）；vt.h 还有 Screen
  坐标（全屏含滚动区），本仓未用。

## 7. 枚举与常量的 bindgen 形态

- C 枚举生成 `pub mod X { pub type Type = c_int; pub const … }`（ModuleConsts）；
  `item_name` 回调剥 `Ghostty` 前缀、变体名剥枚举前缀（gen_bindings.rs 的
  PREFIXES 表）。函数不改名（bindgen 会给 link_name 加 `\u{1}` 前缀，Apple
  链接器不支持，rust-bindgen#1221）。
- 宏常量是 Var：`DA_*`（u16，conformance/机型/特性位）、`KITTY_KEY_*`（u8，
  协议位）、`STYLE_UNDERLINE_SHIFT` 走 allowlist_var；`GHOSTTY_STYLE_FLAG_*`
  是 cast 表达式，bindgen 不生成，位值在 vt.rs `style_flag` 按头文件位序展开。
- `Mode` 是裸 u16：DEC 模式直接用编号，ANSI 模式第 15 位置 1（vt.rs 复刻上游
  编码；判定编码错，模式 set/get 会答错位）。

## 8. 核心行为口径（pin vt-1.0.0，实测或对照上游得出）

- mode 2027 字形簇定宽**默认开**（选 ghostty 的根因）；上游 `Terminal::new`
  还会显式 set_mode_default 加固一次，本仓 vt.rs 同。
- DA 默认自报：DA1 = `CSI ?62;22c`（LEVEL_2 + ANSI color）、DA2 = VT220 固件
  10、DA3 unit 0——本仓覆写 DA1 为 VT102 无特性对齐 alacritty（ADR-051）。
- resize 重排**锚定顶部**（内容留屏、光标随内容移动，kitty 同向）——与
  alacritty 的「锚定光标」差异见 ADR-051 分歧表。
- 滚动历史按**页粒度**修剪（约 400 KiB/页），实际保留行数略高于上限；行数
  上限与字节上限先到者生效（本仓用行数口径，字节上限放开）。
- 制表位表不外露（本仓按 8 列近似）。
- 同步输出 2026：核心即时生效，渲染端定格由 embedder 做（BSU 事件抓帧）；
  **核心自己永不结束 hold**，超时必须手动复位模式（stop_sync）。
- 刻意未接的函数族：选择/手势、搜索、kitty graphics、快照编码、键/鼠标
  编码器、OSC/SGR 解析器、格式化器、unicode width——语义在共享层或首版
  不需要。全量 C API 约 205 个函数，本仓只用 27 个，其余约 178 个未读过。

## 9. 扩充能力的标准路径

1. 读 vt.h 里目标函数/结构的文档注释（获取路径见 §1）；
2. `gen_bindings.rs` 的 FUNCTIONS / TYPES / VARS 加条目 → 再生成（[runbook
   §2](../runbooks/ghostty-bindings.md)）；
3. `vt.rs` 加安全封装（unsafe 止步于此）→ `mod.rs` 消费；
4. 契约测试补场景（`crates/terminal/src/contract.rs`，三后端口径）；
5. 语义拿不准时对照上游封装同版本实现（§10）。

## 10. 对照源与快照声明

- 上游安全封装：cargo registry 的 `ghostty-vt 0.8.0+ghostty.1.4.0-main.83fb956`
  （src/terminal.rs 1813 行、render.rs 872、screen.rs 458、style.rs 537）——
  对应旧 pin 83fb956，尚无 vt-1.0.0 对应版本；对照时注意 1.0 的 ABI 改名
  （formatter/tagged option 化、mode 宏重命名等）在该版本之后。
- 本笔记基于 pin `vt-1.0.0`（ffi/ghostty-vt-sys/GHOSTTY.lock，commit 0fa3d57）。
  更新 pin 后逐节复核，特别是 §2 的 tag 表与 §5 的回调表。
