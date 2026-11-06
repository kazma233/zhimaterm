# ADR-051：ghostty 后端——libghostty-vt 作为第三个终端核心

状态：已接受（2026-11）

## 背景

emoji 表格错位（`⚠️` 等字形簇宽度，ADR-050 有意分歧表第 4 行）在 termwiz 后端
按簇口径修复，但默认后端 alacritty 的 `Term::input` 逐码点定宽且无宽度覆盖
入口，适配层无法补救。libghostty-vt（Ghostty 抽出的 C 终端核心，MIT）默认
开启 mode 2027 字形簇定宽，是簇口径的原生实现，且 DA/DSR/OSC 4/10/11/12/
XTGETTCAP 等查询应答全部由核心内部生成。ADR-050 的后端契约
（`TerminalCore`/`GridView` + 契约测试 + 差分测试）使接入的增量只有
「字节 → 网格」一份适配。

决策：接入为第三个后端，**始终编译**（延续 ADR-050 的「可选但默认全开」，
不做 feature 门控），设置名 `ghostty`；**macOS 先行验证**（libghostty-vt
官方仅承诺 macOS runtime，Linux/Windows 只过构建检查）。

## 决策

### 1. 依赖与构建

- **仓内自写薄绑定**，不用第三方封装 crate：`ffi/ghostty-vt-sys`
  （workspace 成员，`links = "ghostty-vt"`）用 bindgen 按精确 allowlist 对
  官方头文件 `include/ghostty/vt.h` 只生成后端用到的 27 个函数及其传递
  类型，绑定预生成提交（源码按 GHOSTTY.lock 的 sha256 pin，绑定天然稳定，
  日常构建只需要 Zig）；构建脚本移植自 libghostty-vt-sys 0.2.2
  （MIT OR Apache-2.0，裁掉子模块/pkg-config/发布版本校验分支）。
  C API 机制口径的探索笔记见
  [libghostty-vt-api-notes](../context/libghostty-vt-api-notes.md)（扩充触面前先读、二次确认）。
- 不设独立安全封装层：上游封装的事件队列/回调安装/快照行走吸收进适配层
  ——`backend/ghostty/vt.rs` 承载全部 unsafe（userdata、10 个 C 回调、
  tagged get/set、两段式缓冲），`mod.rs` 保持纯适配逻辑。适配层不消费的
  事件（pwd/桌面通知/进度上报）连回调都不装（vt.h：未装即关闭该效果）。
- 构建要求：Zig 0.17.x 在 PATH 或 `ZIG`（自行安装）；首次构建联网下载
  `GHOSTTY.lock` 固定的源码包（sha256 校验），离线构建设 `GHOSTTY_SOURCE_DIR`。
  绑定再生成与更新 pin 见 [ghostty-bindings runbook](../runbooks/ghostty-bindings.md)。
- 构造即关 glyph（kitty graphics）协议：首版需求不含图像协议，程序发来的
  图像序列不分配图像对象。

### 2. 适配要点

- **视口行走渲染缓存**：libghostty 的 `RenderState` 批量快照视口而行读取
  契约是 `&self` 逐行——网格/视口变更后懒刷新一次缓存（物化成中性格子），
  读路径查缓存；越出视口的行（回看时的选择/搜索/转录）走 `grid_ref`
  逐格慢路径。
- **回看偏移映射进视口**：`scroll` 直接移动 libghostty 的视口，
  `display_offset` 由滚动状态推导——视口行恰好是正在看的行，缓存才有意义。
- **同步输出（2026）**：核心即时生效 + 渲染端定格（不同于 vte 的缓冲）。
  后端在 BSU 事件时抓帧定格、ESU 前不刷新，对「缓冲到 ESU」行为等价；
  `advance` 按 `\x1b[?2026h/l` 字面量预切块，同块 `BSU+内容` 也能定格；
  超时兜底 `stop_sync` = 复位模式 2026（官方强制结束方式）。
- **主题默认色推入核心**（契约新方法 `set_theme_defaults`，默认无操作）：
  核心的 OSC 10/11/12 查询分支在没有默认色时直接跳过应答，不知道主题色
  就没有应答；ANSI 16 色一并推入调色板。
- **颜色查询应答转共享层**：核心自答走 `PtyWrite`，后端识别 OSC 4/10/11/12
  应答字节转成 `TermRequest::ColorRequest`，由共享层按主题色格式化——
  应答字节与另两个后端同一套。
- **DA1 对齐 alacritty 声明**（VT102 无特性）：程序按 DA1 分支能力，
  ghostty 默认自报 VT300 级会让同一批 TUI 走不同分支。DA2/DA3 仍为
  ghostty 默认值（契约不锁）。
- 契约另两个小扩展：`set_cell_pixels`（核心自答 CSI 14/16/18t 需要格像素，
  worker 随 Resize 命令传入）。

### 3. 已知有意分歧（不照抄上游/alacritty 口径）

| 项 | alacritty/termwiz | ghostty 后端 |
|---|---|---|
| emoji 字形簇宽度 | termwiz 回写簇口径；alacritty 逐码点 | 核心原生簇口径（mode 2027），与 termwiz 同向 |
| resize 重排锚定 | 内容锚定光标：缩放时溢出行进历史，光标位置稳定 | 内容锚定顶部：全部内容留在屏上，光标随内容移动（kitty 同向）；diff 差分对这 3 个场景只断言结构不变量（行占格不超列数、光标在界内），不做整帧对比 |
| 滚动历史上限 | 精确按行数 | 页粒度修剪（约 400 KiB/页），实际保留行数略高；上报值钳回配置上限，多出的内容不可达 |
| 制表位表 | alacritty 不外露按 8 列近似；termwiz 跟踪真实 HTS/TSD | 核心不外露，按 8 列近似 |
| OSC 应答色 | 共享层按主题色格式化 | 拦截核心应答转 `ColorRequest`，同主题色口径；DA2/DA3/XTVERSION 为 ghostty 默认值 |
| 标题 reset | alacritty 有 `ResetTitle` 事件 | 核心无对应事件，丢弃 |
| 2026 同块定格 | vte 内部缓冲，天然正确 | 预切块覆盖；「BSU 序列跨字节块断裂 + 内容跟在同一块」的边缘仍会早显一帧（150 ms 上限内） |

## 后果

- 表格 emoji 对齐在 ghostty 后端开箱即用（簇口径原生），不依赖 termwiz 的
  回写适配。
- 默认后端仍是 alacritty（ADR-050：真实环境长尾兼容以 alacritty 积累为准）；
  ghostty 后端经真实使用验证后可作为默认候选。
- 契约测试 26 场景 × 3 后端、差分 8 场景（5 个三后端整帧 + 3 个
  alacritty/termwiz 整帧 + ghostty 结构不变量）。
- 全体开发者与 CI 需要 Zig 0.17 构建；这是「始终编译」决策的已知代价。
  好消息是依赖面更小：不再有第三方封装 crate，绑定的 allowlist 就是
  本后端对 C API 的全部触面。
- Linux/Windows 未实测：libghostty-vt 官方仅承诺 macOS runtime，三平台
  运行验证列入后续工作。

## 验证

- `cargo fmt` / `cargo clippy --workspace --all-targets`（0 warning）/
  `cargo test` 全绿（本机 macOS）。
- 契约 26 场景三后端通过；差分 8 场景按上述口径通过。
- 绑定换为仓内薄绑定后：契约/差分全量复跑通过（与替换前逐项一致，
  行为等价的证据）；`ffi/ghostty-vt-sys` 经 `aarch64-unknown-linux-gnu`
  交叉 check 验证构建脚本平台分支（windows-msvc 需在 Windows 本机过，
  zig 交叉无 Windows SDK 是上游同样约束）。
- 本机真实使用验证见提交说明（表格对齐、TUI 冒烟、搜索/选择/转录、
  同步输出兜底、性能对照）。
