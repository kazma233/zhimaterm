# ADR-035：滚动历史上限设置

状态：已接受（2026-10-21）

## 背景

requirements §8 承诺「默认保留 10,000 行历史记录，并允许设置上限」，但
`TerminalModel` 构造 `Term` 时从未设置 `scrolling_history`，隐式用上游默认 10,000
（alacritty_terminal 0.26 `Config::default`），「允许设置上限」一直没有兑现；
终端完备性盘点（§3.3）也把它记为未暴露配置。2026-10-21 批次补齐。

## 决策

1. **设置面板「终端」区加文本输入**，照字号输入的既有模式（`SettingsForm` 文本
   实体 + 应用时 `validate_*` 校验 + 表单底部报错）。范围 **0 – 100,000 行**：
   0 = 不留历史；上限按 80 列每行约 1.5–2 KB 估最坏 150–200 MB，是默认的 10 倍。
   默认 10,000，留空即默认。不做静默夹取，越界明确报错（与字号同一纪律）。
2. **即时生效，新会话与已有会话同一套语义**：新建会话经 `SessionConfig.history`
   传入 worker 构造 `TerminalModel`；已有会话经 `SessionCommand::SetScrollingHistory`
   下发，worker 调 `Term::set_options` 换新 Config。广播路径与 `SetTheme` /
   `SetOptionAsAlt` 完全一致（app 遍历每视图 → 视图转命令 → worker 改模型置脏）。
   调小上限会截掉超出部分的历史，语义如实，不做额外确认。
3. **alacritty `Config` 单点构造**：`osc52: OnlyCopy`、`kitty_keyboard: true`、
   `scrolling_history` 三个显式取值收在一个私有函数，构造与 `set_options` 共用，
   防止两处构造随依赖升级漂移。

## 后果

- `settings.json` 新增 `scrolling_history` 字段（`Option<usize>`，serde default
  兼容旧文件）；`SessionConfig` 加 `history` 字段，调用方在 app 层装配时从设置取值。
- 回放（`replay.rs`）与性能验收用默认构造（10,000），不受影响。
- 设置变更只改后续快照来源，不动 PTY 与连接，无跨模块副作用。

## 验证

`cargo test`：settings 往返与校验边界、`TerminalModel` 构造上限、
`set_scrolling_history` 截断行为（缩小后历史行数变化、快照不炸）。
