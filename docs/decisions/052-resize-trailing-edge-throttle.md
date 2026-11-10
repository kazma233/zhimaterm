# ADR-052：Resize 尾沿节流

状态：已接受（2026-11-10）。

## 背景

Resize 在 UI 的 canvas paint 回调里按几何判重直发（`terminal_view`），拖动
窗口边缘或分屏分隔条时每跨过一个格宽边界就发一条。worker 对每条命令同步执行
三类副作用：

1. 模型 reflow（默认后端 alacritty 的 `Term::resize` 逐行重排，O(历史 × 列)，
   默认历史 10,000 行）；
2. PTY ioctl(TIOCSWINSZ) + SIGWINCH（本地）或 `request_pty_size` 往返（SSH）；
3. 前台程序因 SIGWINCH 全屏重画，输出洪泛回来再叠加解析与快照负载。

实测（2026-11-10，M4/16GB，release）：50,000 行历史、200×50 会话突发
240 条 resize，逐条串行应用耗时 **485.9ms**（≈2ms/条全量 reflow），期间
按键与输出都排在其后。多格同窗口时按格数放大。

另有两处相邻缺陷一并处理：

- **投递丢弃不可自愈**：`Session::resize` 原为 `try_send` + 静默丢弃，而
  UI 判重键 `last_size` 在发送前已更新——命令队列满（256）时该次 resize
  永久丢失，模型/PTY/窗口三者失同步直到下次几何变化。
- **仅队列内合并不够**：worker 空闲时每次唤醒只排到一条 Resize，「排空时
  末值合并」只有在队列积压时才生效，节流必须挂在时间轴上。

## 决策

1. **尾沿节流（`ResizeGate`，纯时间决策核）**：距上次应用已过静默期
   （`RESIZE_QUIET = 50ms`，worker 内常量、不进设置）的命令立即应用；
   静默期内的命令只暂存最新值，到点应用并重置窗口。效果：单次 resize
   （开合面板）零新增延迟；连续拖拽按 ≤50ms 节奏应用最新值（内容持续
   跟进而不是冻结到松手）；停止后最迟一个静默期收敛。
2. **模型与 PTY 同步节流**：`apply_resize` 是唯一副作用点，模型 reflow、
   恢复门控灌入（ADR-049）与 PTY resize 一次完成——一串中间尺寸对三类
   代价各只按节流后的次数付费，SIGWINCH 风暴同步消失。
3. **静默期定时器进等待集合**：暂存期间 worker 的等待集合加入
   `next_wakeup`，到点即醒；无暂存不参与（不破坏空闲零唤醒）。
4. **投递失败回滚判重键**：`Session::resize` 返回
   `Result<(), CommandError>`，UI 只在投递成功时更新 `last_size`，失败
   （队列满）下一帧 paint 重发；已结束会话命令通道已关，Closed 同样走
   不更新分支。SSH pump 侧维持丢弃语义不救——pump 队列满意味着会话在
   走向终态。

### 与 ADR-046/049 的交互

- 启动首条 Resize 经 `wait_startup_size` 消费（主循环之前），不经节流；
  PTY 按它开工的语义不变。
- Armed 窗口的重灌按**节流后实际应用**的尺寸算一次 `lines_grew`
  （`advance_restore_hold` 在 `apply_resize` 内），同帧多条中间尺寸合并
  为最终一条，重灌语义不变。

## 备选方案

- **UI 侧防抖（尾沿才发）**：单次 resize 也被延迟 16ms+，且每个发起点
  （canvas、分屏拖拽、面板开合）都要各自装；worker 单点覆盖所有来源。否决。
- **只做队列排空合并**：worker 空闲时无积压可合并，逐条 reflow 照旧。否决。
- **模型立即应用、只节流 PTY**：reflow 本身就是每条 2ms 级的大头，留着
  它节流收益减半。否决。
- **静默期进设置**：没有真实需求支撑（KISS，少写开关）。否决。

## 后果

- 拖拽期间终端内容以 ≤50ms 粒度跟进，视口几何（UI 布局）即时、模型内容
  晚至一个静默期重排——与所有终端的子进程重画延迟同量级，无感。
- 队列满时 resize 至多多等一帧 paint（≤16ms）重发，不再永久丢失。
- perf 用例（`perf_resize_burst_applies_at_trailing_edge`）守住收敛上限：
  末条命令 → 终尺寸帧 ≤ 300ms。

## 验证

- 单元：`resize_gate_applies_the_first_command_immediately`、
  `resize_gate_stashes_only_the_latest_within_the_quiet_window`、
  `resize_gate_applies_continuous_streams_at_quiet_cadence`（决策核纯时间
  逻辑，帧序 0/4/8 的应用节奏锁定 16ms 命令流 × 50ms 静默期）。
- 性能：`perf_resize_burst_applies_at_trailing_edge`——50,000 行历史 +
  240 条突发，节流前 485.9ms 超限（可复现失败）、节流后 106.8ms 通过，
  历史无丢失；方法与数字见 `docs/runbooks/perf-acceptance.md`。
- 回归：`cargo test` 全绿（含 ADR-049 的灌入门控用例）。
