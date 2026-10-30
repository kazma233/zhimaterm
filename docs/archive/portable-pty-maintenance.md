# portable-pty 采用边界

> 已失效（2026-10）：portable-pty 已被自研 `src/pty/` 模块替换，见
> `docs/decisions/025-selfhost-pty.md`。本文保留作为当初选型与验证要求的
> 历史记录；其中「macOS/Linux 专项验证」的验收要求对自研实现继续适用。

本记录用于 ZhimaTerm 本地 PTY 依赖选型。它只记录稳定的采用边界；版本、维护状态和兼容性应在首次纳入依赖及后续升级时按文末来源重新验证。

## 结论

`portable-pty` 是 WezTerm 项目内的跨平台 PTY 抽象，适合作为 macOS/Linux 优先、Windows 后续支持的本地终端 PTY 候选。

需求规格当前暂定候选版本为 `0.9.0`。不得将 WezTerm 主分支的修复自动视为已经进入可复现的 crates.io 发布物；升级或临时 patch 必须记录来源 commit、原因与回退条件。

建议：首版若采用它，固定 `portable-pty = "0.9.0"` 和 `Cargo.lock`，不依赖 Git `main`；在实现前做 macOS/Linux 验证，覆盖 Shell 启动、resize、关闭会话、子进程回收和持续高输出读取。若该版本有阻断问题，再基于具体 commit 评估临时 patch，而不是常态化跟随主分支。

## 复核来源

- [crates.io 包页面](https://crates.io/crates/portable-pty)：用于确认当前发布版本、许可证和维护者。
- [crates.io 版本历史](https://crates.io/crates/portable-pty/versions)：用于确认候选版本是否仍可用及其发布日期。
- [官方 API 文档](https://docs.rs/portable-pty/0.9.0/portable_pty/)：用于确认 `PtySystem`、`MasterPty`、`Child` 与 `ChildKiller` 的现行接口。
- [WezTerm `pty/` 源码目录](https://github.com/wezterm/wezterm/tree/main/pty)：用于检查未发布修复是否影响选定版本。

## 采用边界

- `portable-pty` 提供的是同步 `Read`/`Write` 风格 PTY 资源抽象，不提供 Tokio 或 smol 原生异步接口。
- ZhimaTerm 使用 GPUI-pre 与 smol，因此应把 PTY 放在专属 worker 中，并通过有界通道回投合并后的终端屏幕更新；不能在 GPUI UI 线程直接读写。
- `Child::kill` 只定义被启动子进程的终止能力。应用仍需通过集成测试确认 Shell 衍生进程的回收是否满足产品的“会话关闭不残留子进程”验收条件。
- 该 crate 只解决本地 PTY；VT 解析、SSH/SFTP、会话调度、屏幕增量模型和 GPUI 渲染仍由终端组件负责。
