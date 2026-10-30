# ADR-045：全局记录门面（recorder）

状态：已采纳（2026-10-22）

## 背景

两类「会话留痕」的埋点都在靠对象穿透：

- **会话输出日志**（ADR-017）：`TerminalView` 建格创建
  `Arc<Mutex<Option<SessionLog>>>`，塞进 `SessionConfig` 交给 `session::spawn`，
  worker 线程持有、每个写点 `lock_log(&log)`——三层穿透（视图 → 配置结构 →
  worker）。未来 worker 更深处（transport 层记连接事件之类）每加一个埋点
  都要把句柄再穿一层。
- **操作记录**（ADR-044）：`TerminalView.operations` 实体在 `SftpReady` 时
  注入 `SftpPanelView`——面板拿到的是可操作实体，写读都直连。

需求：埋点只说「记什么」，记到哪里、怎么写由装配期的全局对象决定；消灭
句柄在方法间穿来穿去；为未来新埋点铺统一入口。

## 决策

### 1. 全局注册表按格路由，key 是视图实体 id

`recorder` 模块持有全局 `RwLock<HashMap<EntityId, PaneRecorder>>`。key 用
TerminalView 的 gpui 实体 id（u64、Copy、跨线程）；每格一条装配
（`PaneRecorder`）：日志 sink、日志路径、来源标签、操作记录实体。

- 注册：建格时 `attach_log`（不开日志也注册——开关是运行期切的，条目得
  先在；注册必须先于 `session::spawn`，worker 线程一起来就查表）；SSH 格
  加挂 `attach_history`（本地格没有写入方与读取方，不再白建实体）。
- 注销：`TerminalView::drop` 摘条目，日志句柄与历史实体随之释放。正常
  时序 worker 线程已随会话退出；异常时序下 worker 的线程局部缓存自持
  sink 引用，摘表不影响它，句柄随线程结束释放——注销无竞态。
- 未注册的格：查询 None、写入 no-op。「该格没有记录」是生命周期边界的
  正确语义，不是吞错。

### 2. worker 侧无参埋点：线程绑定

会话是专用 `std::thread`（不跨线程漂移，事实已核），`spawn` 的线程包装器
入口 `bind_pane(pane)` 把本格 sink 缓存进 thread_local——之后任意深度的
埋点无参调用（`output_line` / `begin_session` / `is_logging` /
`take_log_failure` / `finish_session`），写入零查表。`SessionConfig` 不再
携带日志对象，只带 pane id。

### 3. 两个门面，不是一个 trait

输出日志（worker 线程、高频、文件 IO、失败静默停写）与操作记录（UI
线程、低频、gpui 实体、写后 notify 重渲染）的调用上下文根本不同，trait
签名统一不了，强行统一是为不存在的第三种埋点预付复杂度。两者共享
「注册表 + 按格路由」的模式，各自贴合线程模型：

- 日志门面 UI 侧：`set_logging(pane, on) -> bool`（唯一有返回的——成败
  给视图亮 ⚠）、`logging_enabled(pane)`、`log_path(pane)`。
- 操作记录门面（UI 侧显式带格）：`record_operation(pane, task, cx)`
  （内部 update 实体 + notify）、`history(pane) -> Option<Entity<…>>`
  （历史模态、面板最近条目等读取方）。SFTP 面板注入的从可操作实体降为
  不可变 id，写入只能走门面。

### 4. 装配期配置，运行期只有开关

记到哪里、怎么写（目录、文件命名、标记格式）在注册时定死，由
`SessionLog` / `SessionHistory` 的既有实现承载。运行期唯一动态是日志
开关（ADR-017 语义不变：有文件按原文件追加重开、没有则新建，开关标记
解释空白，不写进任何设置）。

### 5. 失败策略沿用旁路契约

埋点全部无返回（ADR-017「日志绝不拖垮会话」：写失败在 sink 内停写、
经 `take_log_failure` 交出、只报一次）；例外只有 `set_logging` 返回
bool。`LoggingStopped` 控制事件链路原样。

## 后果

- worker 深处未来任何新埋点一行无参调用，零穿透；`SessionConfig` 的
  `log` 字段、`worker::lock_log` 辅助、面板的 `history` 实体字段全部
  删除。
- 本地格不再创建无用的 `SessionHistory` 实体。
- e2e 测试与生产走同一条注册路径（`attach_log` / 注入变体），自造格 id
  必须用 recorder 的全进程唯一自增源——两套独立计数器会跨模块撞 id，
  注入 insert 静默覆盖别人的条目（落地时真实踩过）。
- 注册表是进程级全局态：并行测试靠唯一 id 隔离；`attach_log` 带重复
  注册的 debug_assert。
- 手工验收：SSH 会话开关记录（记录模态）、SFTP 操作落历史（历史模态，
  底栏「历史」按钮高亮提示）、重连后日志继续追加同一文件。
