# ADR-029:跨平台代码的模式档位与抽象时机

状态:已接受(2026-10)

## 背景

macOS 真机到位后,ADR-028 的 unverified 项 libproc 结构体布局实机爆雷
(手写 FFI 布局与头文件不符,进程表恒空,详见 platform-verification)。由此次
事件引出两个一般性问题并做了全项目评审:跨平台代码里「抽接口」是不是好办法;
现有代码里有没有不合适的跨平台写法。本 ADR 固化评审结论。

## 决策

### 手段按差异厚度选档,禁止跳档

| 档 | 手段 | 适用 | 项目内样板 |
|---|---|---|---|
| 1 | 差异即数据:常量性差异建模成表字段(mac/other 配对),代码路径单一 | 键位、路径、文案 | `input.rs` `ShortcutSpec`、`config_paths.rs` |
| 2 | 变体同门控:平台专属概念做 cfg 变体,构造点/匹配臂/UI 入口同门控,穷尽性编译期成立,不留运行时兜底 | 平台独有概念(WSL) | `LocalShell::Wsl`、`Modal::Wsl` |
| 3 | 平台模块整文件 + 装配层零 cfg;std API 名差异允许函数内 2–3 行 cfg 块 | 一整套实现差异,编译期选择 | `probe/`、`pty/{unix,win}.rs`、`poll.rs` |
| 4 | 显式 trait(trait 定义 + cfg 门控 impl,不用 dyn) | 厚差异且契约值得写显 | 暂无(probe 为候选,见下) |
| 5 | 运行时抽象(dyn/泛型后端注入) | 需要**运行期**选择实现 | 无,也不应有 |

跨平台是编译期事实,档 5 与平台差异无关——运行时多态是为「同一构建内多种
实现」准备的(本地 vs 远端、真实 vs 测试假后端),为平台上 `Box<dyn>` 是
过度设计。

### 接口抽象的时机

- trait 与 cfg 模块别名在静态保障上等价(都只检查当前编译的平台),trait 的
  增量收益是契约显式化与缺项报错质量。静态手段的天花板是「一次构建只检查
  一个平台」,完全解只有 CI 三平台 matrix(见 platform-verification 待办)。
- **probe 平台层保持 cfg 模块别名,不抽 trait**。接口事实上已存在
  (`processes()` + `listening_sockets(&tree_pids)` 两个函数),形状已被
  2026-10-19 的修复验证稳定(修实现,契约零改动)。重看信号:远端/WSL 进程
  面板这类第二后端出现时再抽,且按「后端带状态」形状抽(`&self` 方法 +
  构造注入通道上下文),不要按当前的无状态关联函数形状抽——远端后端必然
  持连接句柄,按静态形状抽会改两次。
- 事件流新类型包(`ProbeEvents`/`MonitorEvents`/`ForwardEvents`/`SftpEvents`)
  保持各自独立:共有的只是 enum 外形,payload 完全不同,统一是零收益泛型化。
- 平台差异收敛得越早越好(越靠近构造边界),下游越干净;留在类型内部,cfg
  会跟着类型繁殖。

### 已知可议点(记录,不动)

- `pty::Command` 双面性:Unix 侧(默认 shell 标记 + spawn 时延迟解析)与
  Windows 侧(显式 argv + 完整环境表)共用一个类型,该类型的多数 cfg 都在
  维持这个双面性。下次动 pty 时往「构造边界收敛」方向改:会话层在 Unix 上
  就把默认 shell 解析成显式 argv,让 `Command` 单一语义。不专门改。
- `app.rs` 的 `wsl_entry_visible` 字段/初始化/使用三处门控,比变体同门控
  略散,属可接受。

## 后果

- 新增跨平台代码按档位对号入座;评审时「越档」(薄差异上模块、编译期选择
  上 dyn)视为问题。
- 现有代码保持现状,本 ADR 不触发重构。
- 平台实现项的完成定义包含「该平台实机 `cargo check --all-targets` +
  `cargo test`」(platform-verification 的纪律,libproc 事件为其反面实证:
  交叉 check 抓类型错误,布局这类运行期契约只有实机暴露;`cargo check`
  不编译 `#[cfg(test)]` 也是盲区,`--all-targets` 必须带上)。
