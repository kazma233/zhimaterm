# 平台验证现状

状态：临时事实记录（验证节奏），平台口径本身见 README——Windows、Linux、macOS
均为目标平台、一等公民。

## 现状（更新至 2026-10-29）

- **Windows**：在 Windows 平台上验证，日常构建与 e2e 均在此完成。2026-10-22 首次全量
  `cargo test`（此前只有 `cargo check --all-targets`）：312 passed / 36
  ignored / 5 failed——5 个失败是既有的 settings fixture Unix 路径假设
  （master 基线同样失败，已记 todo 技术债），非回归。同日补齐并实机验证：
  PEB 读取（cwd/argv，`peb::` 测试对照 GetCommandLineW）、probe 端口归属
  （`own_listening_sockets_appear_in_the_sample`：GetExtendedTcpTable 参数序
  与字节序实证）、`cargo check --all-targets` + `cargo fmt --check` 通过。
- **Linux**：在 Linux 平台（Fedora）验证，2026-10-18 完成 Linux 首次全量
  `cargo check --all-targets`（此前 WSL 批次代码只验证过 Windows，翻车记录见
  ADR-021 附记）。`cargo test` 与 perf 套件在 Linux 的运行记录仍缺
  （unverified）。
- **macOS**：真机到位（Apple Silicon），2026-10-19 首次构建 + `cargo test`。
  首查即暴露三处，全部修复后测试全绿（218+5）：
  - edition 2024 硬错误：`extern "C"` 块须写 `unsafe extern "C"`（probe/macos.rs）。
  - libc 0.2.190 把 apple 分支 `openpty` 的 winsize 参数 `*const`→`*mut`
    （openpty 会写回实际生效尺寸），改传 `&mut ws`；Linux 各分支签名未动，
    glibc/musl/uclibc 已用最小 crate 交叉 `cargo check` 验证兼容。
  - ADR-028 unverified 项 **libproc 结构体布局实机爆雷**：手写的
    `ProcBsdInfo`/`ProcTaskInfo` 与 sys/proc_info.h 不符（72/136、56/96 字节，
    漏 rfu_1、pbi_name、start_tv 是 u64 等），`proc_pidinfo` 返回值永不等于
    手写大小 → 进程表恒空 → 面板报「Shell 进程已不存在」而 shell 实际存活。
    修复：删手写结构体与 `extern "C"` 声明，改用 libc 导出的 `proc_bsdinfo`/
    `proc_taskinfo`、flavor 常量与 `proc_*` 函数（手写 FFI 契约整块归零）；顺带补
    `ListeningSocket` 的 `PartialEq`（macos.rs 测试的
    `assert_eq!` 需要它——意味着探针的测试代码此前从未在任何平台编译过，
    是 `cargo check` 不编译 `#[cfg(test)]` 的盲区实证）。
  - probe 模块（ADR-028）此前在 Linux 的交叉 check（2026-10-18）抓的是
    Windows 侧类型错误；libproc 布局错误类型检查抓不到（布局是运行期契约），
    只有实机运行暴露。
  - 同日新增 argv 采集实机验证通过：sysctl `KERN_PROCARGS2` 取全量 argv +
    解释器脚本名恢复（跑 `pi` 的 node 进程显示 pi），probe 测试 13 项全绿，
    含自身 argv 读取与 shebang 脚本端到端（名字显示脚本而非解释器 sh）两例；
    命令行口径与同类工具用 sysinfo `cmd()` 取到的结果一致（同一系统接口）。
  - 2026-10-21：全量 `cargo test` **285 passed / 31 ignored**（忽略项是需要外部
    sshd 的 SSH e2e），`cargo check --all-targets`、`cargo fmt --check` 通过；
    覆盖 ADR-035~040（布局持久化、重启恢复、全彩回滚）。

### 2026-10-23 之后的批次（2026-10-30 补记，证据为提交记录）

- **2026-10-24（Windows + WSL tester）**：SSH 转发/跳板/认证叠加 e2e 全量复验，
  docker 形态 **9/9 通过**（含 WSL 直连形态下失败的 jump+转发用例闭环）；同轮
  修复 wrong_password 用例 bug、转发用例读超时与 runbook 滞后（记录见
  `../archive/ssh-e2e-wsl-tester-verification.md`，含 SOCKS5 与运行中增删的验证）。
- **2026-10-27~29 批次**（ADR-044~048、连接期密码询问全链路、gpui-pre 0.3.10
  升级、views()/transcript/reconnect 系列重构）：3f3093f 提交信息记录
  「单测 372、门控 SSH e2e 26 全绿」；探针结束进程 macOS 已实机验收
  （`manual-acceptance.md` 2026-10-28 小节）。
- Windows/Linux 在上述批次**无实机验证记录**（unverified）：d135f9d 自述
  「Windows 侧编译门控 unverified」，61d254f 升级 gpui-pre 后需 Rust ≥1.98
  的工具链前提也未在两端复核。

## 待办

已聚合至 `todo.md`（2026-10-21，唯一入口）。

## 纪律

改 `#[cfg]` 分叉的代码，双平台 `cargo check --all-targets` 通过才算完
（ADR-021 附记的教训）；键位表改动跑 `bindings_have_no_duplicate_keystroke`。
