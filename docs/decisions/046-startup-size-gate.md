# ADR-046：会话在尺寸就绪后才开工

状态：已接受（2026-10-23），2026-10-30 由 ADR-049 修订。补充 ADR-039/040
的恢复口径：恢复机制不变，把「恢复发生在什么尺寸上」补成显式契约。

> **修订（ADR-049）**：本文的「到点兜底用 fallback 开工」只保留 PTY 出生
> 尺寸的语义；超时后**不再灌入回滚**——灌入等首个 Resize（尺寸权威信号）
> 到达，灌入后、首字节前的增行 resize 由门控清掉重灌。非活跃 tab 等不到
> 首帧 Resize 的路径本文未覆盖，是那次修订的起因，细则见 ADR-049。

## 背景

会话启动固定以 `SessionConfig` 里的 96×30 起步，worker 立刻 open PTY、
建终端模型、灌回滚，之后 UI 首帧 layout 量出真实行列才发 Resize 修正。
实测（2026-10-23 排查「恢复后宽度莫名变化」）发现两个后果：

1. **恢复内容被 resize 吸回可见屏幕。** 恢复灌进滚动历史的内容，在第一次
   resize 行高变大时（真实行高几乎总是大于 30）被 alacritty 的屏幕向上扩
   张整个拉回视口，`history` 从 8 归零、内容钉在屏幕顶部——违背 ADR-039/040
   「恢复内容只进历史、新提示符从空屏开始」的承诺。对照实验：只加宽历史
   保全，只加高历史归零。
2. **子进程出生在假尺寸上。** PTY 的行列是子进程的出生属性（openpty 的
   winsize、SSH 的 pty-req），shell 与终端里的 AI CLI 以 96 列开场，等
   SIGWINCH 后重排；inline TUI 按旧锚点局部重绘时画错位置，用户看到
   「宽度莫名变化」。

「连接期间 UI 可能已报尺寸」的 `pending_resize` 只在 SSH 信任弹窗等待期间
才可能生效（赋值点只有 `ask_trust`），本地格与绝大多数 SSH 会话都落在
96×30 上。原生终端普遍是「窗口 → 字体度量 → 算行列 → 开 PTY → 起 shell」
的天然顺序（`alacritty_terminal` 的 `tty::new` 在 openpty 时就要求行列）；
本项目是 view 异步 layout 把这个顺序拆开了，需要显式补回。

## 决策

1. **尺寸就绪再开工**：worker 在 `transport::open` 之前先过「初始尺寸阶段」
   （`wait_startup_size`）：
   - `StartupSize::AwaitLayout`（UI 起的格）：等待首个 `Resize` 命令，上限
     `STARTUP_SIZE_WAIT`（250ms）；同帧连发的多条 Resize 取最后一条；到点
     兜底用 `fallback`（96×30，只在 layout 迟迟没有发生时走到）。
   - `StartupSize::Fixed`（测试与探针）：直接开工，不经等待，命令原样留在
     队列里给主循环排空——尺寸的权威在调用方。
2. **等待期间的命令不丢**：非 Resize 命令（主题等设置在 spawn 后立刻发出，
   早于首帧 Resize）由门控原样带回，在主循环前按既有语义补处理；`Close`
   （含通道关闭）立即终止——此时无可收尾资源，发 `Exited(Closed)` 即返回，
   回滚文件不能拿空模型覆盖上一份。
3. **open、建模型、灌回滚全部用就绪后的尺寸**：PTY 出生即在真实行列；恢复
   内容按真实宽度一次折行进历史。信任弹窗等待期间的后续 Resize 仍由
   `pending_resize` 捕获（覆盖模型初始尺寸），语义不变。

## 备选方案

- **恢复后行高变大时重新执行滚入技巧**：针对现象的补丁——「恢复发生在错
  误尺寸上」的规则缺陷还在，且要区分视口里是恢复内容还是 shell 已打出的
  提示符，边界模糊。否决。
- **文件头存上次尺寸，按它建模型**：行高不同照样吸历史，不解决根因；只在
  宽度恰好一致时省一次 reflow。否决（将来可作诊断信息附带存）。
- **view 延迟 spawn 到首帧 layout**：更「源头」，但隐藏 pane 不参与 layout
  时会变成「切过去才连」，改变工作区恢复语义。先落 worker 侧门控，之后如
  需收敛再评估。

## 后果

- 本地会话启动多等约一帧（~16ms），SSH 会话在握手前等同一周期，无感。
- e2e 测试统一走 `StartupSize::Fixed`（尺寸权威在测试），不烧等待上限。
- 恢复格的可见行为回到 ADR-039/040 的承诺：新会话空屏起提示符，旧内容在
  历史里；终端内程序从第一个字节起就在真实尺寸上，启动期不再有 SIGWINCH
  重排。
- 用户此后手动调整行高时，屏幕向上扩张吃历史是所有终端的标准行为，不再
  属于缺陷。

## 验证

- `cargo test worker::tests`：首个 Resize 定尺寸、同帧连发取最后、超时兜
  底、等待期关闭返回 None、Fixed 不吞队列。
- `cargo test restored_scrollback_stays_in_history_and_regains_width_after_resize`：
  模型层——窄模型恢复后只加宽，内容留在历史、满宽背景行按新宽度复原。
- `cargo test restored_scrollback_opens_at_the_delivered_size -- --ignored`：
  e2e——上次会话 120 列的彩色输出，新格以 AwaitLayout 启动并立刻上报
  120×40，首帧 `columns == 120`、`history_size > 0` 且视口无恢复内容。
- 手工：恢复格重启后提示符从空屏顶部开始，旧输出在上方历史；终端内
  AI CLI 的消息框宽度与重启前一致。

## 修订（2026-10-30 补记）：`pending_resize` 现有两个赋值点

连接期密码询问（ADR-018 修订，3f3093f）把信任确认与密码询问收进同一个等待
回路（`ConnectAsks`）：`UiAsks::password` 等待期间的后续 Resize 同样经
`pending_resize` 捕获。背景与决策 3 中「赋值点只有 `ask_trust`」是 2026-10-23
时点的事实，现应读作「每个等待型询问各有一个赋值点」；覆盖模型初始尺寸的
语义不变。
