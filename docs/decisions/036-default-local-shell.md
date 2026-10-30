# ADR-036：默认本地 Shell 与工作目录

状态：已接受（2026-10-21）

## 背景

requirements §3.2 承诺「用户可选 Shell、工作目录和终端尺寸」，但 `LocalShell`
只有 `Default`（Windows 多一个 `Wsl`），侧栏「本地」入口硬编码 `LocalShell::Default`，
`pty::Command` 没有 cwd 字段；Unix 默认 Shell 以登录 Shell 启动并落在 home，
用户无法改。终端尺寸一项经核对没有落点（见决策 4）。

## 决策

1. **设置面板加「默认本地 Shell」与「默认工作目录」两项**。Shell 用无字体预览
   的普通下拉：候选来自系统登记表（Unix `/etc/shells`，过滤注释、不可执行项
   并去重；Windows 按 PATH 探测 pwsh / powershell / cmd），首项「系统默认
   （$SHELL）」，末项「自定义…」切绝对路径输入。工作目录是文本输入，留空 = home。
   两项都在「应用」时校验（Shell 必须是**绝对路径**的存在的可执行文件，目录必须
   存在且为绝对路径），不合法当场报错、不落盘。绝对路径是硬要求：spawn 会先
   切到 home/工作目录再 exec，相对路径会按新目录解析（设置时通过、启动时失败）；
   Windows 只接受 `.exe`/`.com`——`.bat`/`.cmd` 需要 `cmd /c` 包装，接受它只会
   把失败推迟到会话启动。
2. **侧栏「本地」入口保持一键开**，用设置里的默认值；不弹选择浮层，也不做
   右键「用其他 Shell 打开」的临时覆盖。变更只影响之后新建的格，已开的
   shell 不动。
3. **设置指定的 Shell 以登录 Shell 语义启动**：argv[0] 前缀 `-`、补 `SHELL`、
   cwd 用设置的工作目录或 home。与默认 Shell 的唯一差别是程序路径由用户
   给定，读 profile 的行为一致——否则用户换 shell 后会得到与默认 shell
   完全不同的 PATH 与别名环境。Windows 没有登录 Shell 概念，直接启动；
   用户选 pwsh/powershell 时与默认分支一样补 `-NoLogo`，横幅不占开屏行。
4. **不做「初始终端尺寸」设置**：zhimaterm 的格子尺寸由标签/分屏布局决定，
   spawn 后首帧就会按视图实测尺寸 resize，用户指定的初始尺寸活不过一帧。
   该需求来自 MobaXterm 的独立终端窗口语义，在分屏模型里没有落点；§3.2
   改写为「尺寸随视图」。这是明确取舍，不是遗漏。
5. **路径不静默回退**：手改配置写进不可用路径时，由会话启动失败在格中央
   显式报错（复用既有启动失败态），不悄悄退回默认 Shell——静默回退会让
   用户以为设置生效了。

## 实现结构

- `LocalShell` 扩为：`Default { cwd }` / `Program { program, cwd }`（+ Windows 的
  `Wsl`）。cwd 随会话目标走，分屏新格自然沿用同一 Shell 与目录（ADR-014）。
- `pty::Command` 新增 `cwd` 与 Unix 的 `login_shell(program)`；Unix 的
  `build_command` 两个登录分支共用一个 `login_arg0`，显式 cwd 覆盖 home 落点；
  Windows 的 `CreateProcessW` 用命令上的 cwd 覆盖 USERPROFILE 落点。
- Shell 候选单点在 `shells` 叶子模块；设置校验单点在 `settings`，UI 不自行判断。
- 本地探针（ADR-028）对 `Program` 格同样启用：用户指定的 Shell 仍是宿主进程树，
  只有 WSL 格枚举不到。

## 验证

- `cargo test settings::`：Shell/工作目录校验的正反例（空 = 默认、不存在、
  非可执行、非目录）。
- `cargo test pty::unix`：指定工作目录覆盖 home 落点。
- `cargo test configured_shell_and_workdir_take_effect`：真实会话里 `/bin/sh`
  在配置目录下启动，`pwd` 输出该目录。
- 手工：设置里切换 Shell/目录 → 应用 → 新建本地会话确认生效；重启后设置保留。
