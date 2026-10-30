# ADR-021:WSL 会话是本地 Shell 的一种(Windows)

状态:已接受(2026-10)

## 背景

Windows 上用户日常在 WSL 里工作,ZhimaTerm 需要能直接开 WSL 会话。参考 Windows
Terminal 的实现:WSL 在那里不是一种连接,只是一个 profile——连接层自始至终只有
ConPTY,WSL 会话就是 `CreateProcess` 启动 `%SystemRoot%\System32\wsl.exe -d <发行版>`,
与 PowerShell/cmd 完全同路径。这与本项目的传输层设计吻合:worker 只认「读、写、
改尺寸、判断结束」,WSL 不需要新传输层。

发行版枚举有两个已验证的事实约束:

- Windows Terminal 的生成器读注册表 `HKCU\...\Lxss` 但跳过 `Modern=1` 的项,而新版
  WSL(Store 版 `wsl --install`)注册的发行版全带 `Modern=1`,照抄会列出空表
  (本机 `dev` 发行版实测如此)。发行版本身仍在 Lxss 注册表里。
- `wsl.exe --list` 的输出在重定向下是 UTF-16LE 且表头随 locale 变化,解析脆弱,
  还要拉起进程。

## 决策

### 建模:Local 携带 shell 目标

`SessionTarget::Local` 从无数据变体改为携带 `LocalShell`:

- `LocalShell::Default`:现状的登录 Shell(Windows 上 pwsh/powershell,其余平台
  login shell),行为不变;
- `LocalShell::Wsl { distro }`:WSL 发行版。

不新增 `SessionTarget::Wsl` 变体:传输层 WSL 与本地 shell 是同一条路径
(`local::open` 参数化子命令即可),「本地格语义」的判定(`Local { .. }`)天然涵盖
WSL,不必在每个 match 点写 `Local | Wsl`。

### 会话语义:完整套用本地格(ADR-016)

WSL 格无底栏、shell 退出即自动收格、无原地重连、关闭不二次确认、不提供会话日志
开关、无 SFTP/监控/转发。`wsl.exe` 退出即子进程退出,现有的 ConPTY 退出回收
(`child_exit_signal`)原样适用,不引入新的生命周期规则。侧栏来源标签显示
`WSL · <发行版名>`(与 `LOCAL`、`user@host:port` 同一位置同一样式)。

### 启动命令

`%SystemRoot%\System32\wsl.exe -d <发行版名> --cd ~`:

- 显式 System32 全路径,防 PATH 劫持(Windows Terminal 同款约束);
- `--cd ~` 让会话落在 Linux home(WSL 官方 Terminal fragment 与 Windows Terminal
  均如此);不传则落在应用 cwd 的 `/mnt/c/...` 翻译;
- 按 `-d <名字>` 启动而非 `--distribution-id {guid}`:前者兼容所有 WSL 版本,后者是
  新参数;发行版改名后重开会话即可,不值得为抗改名牺牲兼容面;
- `TERM=xterm-256color` 复用本地路径的既有设置。

### 枚举:注册表现查,包含 Modern 项

读 `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`(经 `windows-registry`,
该 crate 已在依赖树):

- 枚举 `{GUID}` 子键读 `DistributionName`,**保留** `Modern=1` 的项;
- 过滤 `docker-desktop*` / `rancher-desktop*` 前缀(Docker/Rancher 的工具发行版,
  非用户目标,Windows Terminal GH#3556 同款);
- 根键 `DefaultDistribution` 标默认发行版;
- 每次点开 WSL 入口时现查,不缓存:注册表读取是微秒级,新装/卸载发行版下次点开
  即见,不需要刷新逻辑;
- 根键打不开(未装 WSL)视为零发行版,入口隐藏。

### 入口与交互

- 侧栏顶栏「会话」标题旁放「本地」「WSL」两个入口;非 Windows 构建没有 WSL 入口
  (编译期 `#[cfg(windows)]`,不是运行时置灰)。
- 底部 footer 的 ⚙ 位置大小不变,「SSH 会话」独占原有两个按钮的宽度。
- WSL 入口点击:一律弹出**模态浮层**让用户选定发行版后再开会话——单个发行版也不直开，
  会话的建立始终经过一次显式选择;浮层复用 `panel::modal` 与模态互斥机制,不做锚定按钮的
  popover——项目无此组件,不为选择器新增布局机制;零发行版隐藏入口。浮层条目两行:主行
  发行版名(等宽字体),副行 `Ubuntu 26.04 · WSL2`(`Flavor`/`OsVersion`/`Version` 缺哪项省
  哪项);默认发行版置顶并带「默认」标记,其余按名称排序;点条目即开会话,Esc/点遮罩取消,
  纯鼠标操作。

### 分屏跟随

WSL 格分屏(ADR-014)新格开**同一发行版**,与 SSH 格同目标新连接的规则对齐;
`TerminalView` 因此保存本格的 `LocalShell`。

## 后果

- `windows-registry` 成为 Windows 目标的直接依赖(锁定 lockfile 已有版本 0.6.1,
  不引入新代码到构建树)。
- 测试 seam:`select_distros` 纯解析层(fixture → 发行版列表:过滤、默认标记、排序、
  Modern 保留)、`launch_args` 命令构造(字面量断言 program 与参数)、`description()`
  来源标签;UI 接线不设单测(与仓库现状一致)。端到端门控用例(`#[ignore]`)在真实
  WSL 上验证发行版环境变量、起始目录、退出回收与分屏跟随。
- 需求规格 §3.2/§3.3 与 README 同步更新。
- 键盘快捷键不为 WSL 提供(现状 SSH 也没有专用快捷键);若将来做会话入口快捷键,
  一并考虑。

## 附记(2026-10-18):`Wsl` 变体收敛为编译期 Windows-only

首批实现把 `LocalShell::Wsl` 变体跨平台定义、仅把 `shell_command` 的匹配臂
cfg(windows),注释假设「其余平台该分支不可达、不参与穷尽性」——假设错误:cfg
只删臂不删变体,Linux 首次编译即 E0004。修正与「入口不做运行时置灰」同源:
**变体本身 `#[cfg(windows)]`**(与 `Modal::Wsl` 同款),全部构造点与匹配臂
(`shell_command`、`description`)与变体同门控,两侧穷尽性自然成立,不引入
`unreachable!` 运行时分支。同批收口 Linux 首编暴露的其余 cfg 分叉问题:
`is_executable` 的 unsafe 块臂体解析、两处 Windows 专属写路径导致的
`unused_mut`、pty-probe 的 Windows-only 条目未门控。

教训:改 `#[cfg]` 分叉的代码,双平台 `cargo check --all-targets` 通过才算完;
本次全部问题都是只验证了 Windows 一侧的直接后果。

## 附记(2026-10-21):入口移至侧栏底部动作行

「本地」「WSL」不再放在顶栏「会话」标题旁;两者与「SSH」一起进入侧栏底部的
单行动作行,WSL 仍仅 Windows、零发行版隐藏。设置入口移到顶栏右侧,主题入口
只保留三点色板。完整形态与取舍见 ADR-041。
