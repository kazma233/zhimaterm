# 聚合待办（唯一入口）

本文件聚合 zhimaterm 全部待办：功能池、人工验收、平台验证、技术债、明确不做/后置。
新待办先入此处再排期；完成的条目直接删除（细节文档如需留档自行维护，不在此重复勾选）。
建立于 2026-10-21。此前待办分散于 platform-verification、manual-acceptance、
perf-acceptance §5、mobaxterm-gap-and-priorities，现以本文件为唯一入口。

## 待人工验收（合成输入进不了 gpui 窗口）

具体步骤见 `manual-acceptance.md`（该文件全空后删除）：

- Windows cwd 恢复、新开格不恢复旧输出、probe argv/可执行路径三项（2026-10-22 小节）。
- 探针结束进程（2026-10-28 小节，ADR-028 修订；macOS 已实机验收通过，
  Linux/Windows 剩余两端各验一遍，无机器时可跳过——类型已交叉 check）。
- 自动重连倒计时逐秒递减（2026-10-29 小节）。
- 侧栏会话行重命名（2026-10-27 小节，ADR-047）。
- 连接弹窗列表优先与连接名称（2026-10-27 小节，ADR-048）。
- 输入即保存与钥匙串读取时机（2026-10-29 小节，ADR-018/048 修订）。
- 连接期密码回退（2026-10-29 小节，ADR-018/048 修订）。
- 询问门与身份随行（2026-10-29 小节，ADR-018 修订补记；键盘交互-only 形态
  优先真机验收，测试容器改制只作降级路径）。
- PowerShell 下「cd 后退出」的目录恢复无平台途径（Set-Location 不落进程
  cwd，ADR-039 修订段）；要覆盖它需 shell integration（OSC 7 被动监听或
  prompt 注入），是否立项待定。

## 平台验证

- macOS 手工冒烟：键位惯例、菜单、字体回退、探针面板实机显示（libproc 修复后
  需重启应用确认；lsof unverified）。
- Linux 侧只记录过 `cargo check --all-targets`（2026-10-18），`cargo test` 与 perf
  套件在 Linux 的运行记录缺失（unverified）。2026-10-21 批次起的新 `#[cfg]` 分叉
  （ConPTY cwd 覆盖、`LocalShell::Program` 的 Windows 启动分支、WSL 格持久化恢复、
  2026-10-22 的 peb/probe 改动）Windows 侧已随当日 `cargo check --all-targets` +
  `cargo test` 全量验证；Linux 侧仍缺一次同款验证（本机 Linux 交叉 check 卡在
  wayland 系统依赖）。
- 2026-10-22 技术债批次（settings 测试 fixture 按平台取路径、known_hosts 文件锁
  与原子写，详见 ADR-002 落盘串行化段）macOS 已全量 `cargo test`（326 过 0 挂）；
  含 `#[cfg]` 分叉与 std 文件锁，Windows 待实机 `cargo check --all-targets` +
  `cargo test`——此前失败的 5 个 settings 用例应转绿，known_hosts 的 4 个新用例
  （合并、幂等、锁互斥 ×1 + forget 原有 2 个）应同过。
- 2026-10-23 之后的批次（连接弹窗列表优先、侧栏重命名、探针结束进程与详情弹层、
  连接期密码询问全链路、gpui-pre 0.3.10 升级）只在本机（macOS）验证过；
  Windows/Linux 无实机记录（Windows 侧编译门控 unverified）。
- CI 三平台 matrix：暂缓（见「明确不管/后置」）。

## 功能池 P2 剩余

Zmodem（rz/sz）、GBK/编码切换、X11 转发、chmod/属主编辑、传输限速/断点续传/并行、
会话分组、密钥生成器、快捷键自定义/多窗口；另有兼容性小项：小键盘
APP_KEYPAD 编码（GPUI 不区分小键盘按键，缺能力；自 alacritty-lessons 调研
归档时移入）。（手动重命名已于 2026-10-27 落地，ADR-047。）
（背景与排序见 `mobaxterm-gap-and-priorities.md` §3 P2。）

## 技术债

（空。2026-10-22 的两条——known_hosts 并发写丢失更新、settings 测试 fixture 的
Unix 绝对路径——已修，见 ADR-002 落盘串行化段与 `settings::test_paths`。）

## unverified

- 连字（ligature）跨格成形未实测。
- 彩色 emoji 三平台字体回退表现。
- macOS lsof 实机输出（Windows 侧 lsof 对应项——GetExtendedTcpTable 参数序与
  字节序——已于 2026-10-22 用实机测试 `own_listening_sockets_appear_in_the_sample`
  验证）。
- known_hosts 跨进程文件锁的实机并发（Windows LockFileEx 的 per-handle 语义）；
  进程内合并、幂等、锁互斥已有单测守住（ADR-002 落盘串行化段）。

## 明确不做 / 后置 / 不管

- **后置（确知当前做不了）**：SFTP 覆盖上传的原子提交。vendored libssh2 1.11 不分发
  posix-rename@openssh.com 扩展，`OVERWRITE|NATIVE` 仅 SFTP v5+ 生效；OpenSSH 等 v3
  服务端上覆盖上传总是回退直写（requirements §6 已按此口径改写，2026-10-21）。
  后续升级 libssh2 或换 SSH 栈时重评。
- **不管（2026-10-21 决定）**：CI 三平台 matrix（requirements §2 的要求保留，不排期）。
- **已落地（2026-10-30）**：固定 Rust 工具链（`rust-toolchain.toml`，编译期下限见
  `Cargo.toml` 的 `rust-version`）——原「不管」条目已按可复现构建的要求补上，
  CI matrix 部分仍按上面的口径不排期。
- **明确不做**：见 `mobaxterm-gap-and-priorities.md` §3 P3（本地编辑器编辑远端文件、
  命令片段、SFTP 双栏/拖放、Telnet/串口决策项、RDP/VNC/XDMCP/网络工具箱、内嵌
  X server、Mosh、`~/.ssh/config` 兼容、背景透明未立项）。
