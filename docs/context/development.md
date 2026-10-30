# 开发指南

构建、运行、测试与代码结构（自 README 移入）。README 只保留产品功能介绍；
功能行为细节见 [features.md](features.md)，模块边界的决策依据见
[ADR-007](../decisions/007-module-boundaries.md)。

## 构建与运行

- 开发运行：`cargo run --bin zhimaterm`——启动可交互的本地终端（左侧栏多会话，支持的全部交互见功能行为说明）。
- 安装到当前用户：`task install`（先构建 release，再按平台做桌面集成：Linux 装进 `~/.local`、macOS 组装 .app 到 `~/Applications`、Windows 注册开始菜单）；卸载 `task uninstall`。
- `cargo run --bin zhimaterm-ssh-sftp-probe`：验证单条 SSH session 中非阻塞 Shell 与 SFTP 的交替推进。该命令只使用 SSH Agent，且要求显式设置 `ZHIMATERM_SSH_ADDR`、`ZHIMATERM_SSH_USER` 和 `ZHIMATERM_SSH_HOST_KEY_SHA256`。不会读取、保存或打印密码、私钥或 `known_hosts`。

## 测试与验证

- **收尾检查（改了代码就跑）**：`cargo fmt` → `cargo clippy --workspace --all-targets` →
  `cargo test`，三项全绿才算改完。clippy 对本仓代码要求 0 warning；`--all-targets` 不能省，
  否则 `#[cfg(test)]` 里的代码不参与编译与 lint。改了 `#[cfg]` 分支的代码，还需在对应
  平台上过一遍 `cargo check --all-targets`。
- 测试分层：小型单元测试随模块内联（`#[cfg(test)] mod tests`，不需要外部环境，`cargo test` 直接跑）；
  **依赖真实 PTY/sshd 的重量级用例集中在 [`src/tests/`](../../src/tests/)**，全部 `#[ignore]` 门控：
  - 会话端到端：`cargo test --bin zhimaterm "tests::session_e2e::ssh" -- --ignored --test-threads=1`（先按 [runbooks/ssh-e2e](../runbooks/ssh-e2e.md) 起 sshd）。
  - WSL 会话端到端：`set ZHIMATERM_WSL_DISTRO=<发行版名> && cargo test wsl_session -- --ignored`（验证进入指定发行版、落在 Linux home、退出即收，ADR-021）。
  - 性能验收：见 [runbooks/perf-acceptance](../runbooks/perf-acceptance.md)（必须 `--release`）。
- 录制流回归：`src/terminal/replay.rs` 按真实读取节奏回放字节流，检查我们自己那层的不变量（快照的段宽自洽、连续快照一致、回看不越界、选区可复制）。默认跑文件内的确定性语料；外部录制（例如 `alacritty_terminal` 自带的 `tests/ref/*/alacritty.recording`）用：

  ```
  ZHIMATERM_RECORDING_DIR=<目录> cargo test --bin zhimaterm replays_external_recordings -- --ignored --nocapture
  ```

  实测：45 段真实录制（956 KB）全部通过，用时约 168 ms。

## 模块划分

按 ADR-007 的分层分组，后面的组可以使用前面的组，反向即违规：

```
── 叶子层（不依赖任何上层）──
src/config_paths.rs            应用配置目录（主机密钥记录与保存的连接都在这里）
src/shells.rs                  系统 Shell 候选列表（/etc/shells 或 PATH 探测，ADR-036）
src/output_log.rs              会话输出日志：文件分配、首尾标记、原始字节追加与故障停写（ADR-017）
src/font.rs                    字体领域：等宽候选实测、回退链、格尺寸测量（CellMetrics）与默认字号
src/input.rs                   输入提取与语义类型：KeyInput、鼠标手势、快捷键绑定（keymap）与滚轮判定
src/keychain.rs                OS 安全凭据存储：凭据按条目 id 挂钥匙串（ADR-018 修订）
src/peb.rs                     Windows 跨进程读 PEB：他人 cwd 与完整 argv（供 cwd 与探针用）
src/pty/                       自研本地 PTY（ADR-025）：ConPTY/openpty、MSVCRT 引用与退出唤醒
src/probe/                     本地进程探针（ADR-028）：三平台采集（/proc、libproc+lsof、ToolHelp+IpHelper）与结束进程（SIGTERM/TerminateProcess），树构建/树内过滤/端口合并的公共装配在这里
src/wsl.rs                     WSL 发行版枚举与启动命令（ADR-021，注册表现查、含 Modern 项）
src/cwd.rs                     本地 shell 工作目录采集（Linux /proc、macOS lsof，ADR-039）
src/sftp.rs                    SFTP 协议：SftpHandle 命令句柄、任务事件流、远端条目模型
src/ui/                        自己写的 UI 组件（与终端无关，可被别处直接复用）
src/ui/input/                  文本输入组件：调色板、表单行助手（field，错误贴标签）与模块组装；core.rs 是编辑核心，ime.rs 是 IME 组合区状态机
src/ui/overlay.rs              统一浮层栈：全窗口模态的唯一归属，栈序即 z 序（ADR-043/044）
src/ui/dialog.rs               通用确认/输入/密码对话框（ADR-044/048）
src/ui/card.rs / panel.rs / menu.rs / select.rs / dock.rs   卡片四区、模态遮罩与按钮、右键菜单、收起式下拉、右缘停靠面板外壳
src/ui/canvas.rs               组件底座：鼠标监听、输入法处理器、bounds 记录

── 模型层 ──
src/terminal/                  终端模型：VT 状态、滚动、选区、输入编码、run 快照
src/terminal/transcript.rs     行转录的格式编解码与逻辑行装配（输出日志与回滚共用，ADR-007 修订）
src/terminal/colors.rs         VT 颜色 → RGB 与颜色查询
src/terminal/replay.rs         录制流回归
src/session/mod.rs             会话协议：命令/事件、目标（本地/SSH）、句柄
src/session/worker.rs          会话 worker：与后端无关的循环、终端模型与回收
src/session/reconnect.rs       重连计划（ADR-013 修订）：资格×退避×上限×取消×看门狗的纯状态机
src/session/scrollback.rs      本地格回滚持久化：版本化 JSONL 与旧格式回退、文件命名契约（ADR-039/040）
src/session/transport/         传输后端：本地 PTY、SSH 多路复用 pump、跳板桥、端口转发、agent 转发、远端监控
src/session/transport/ssh/     SSH 传输（libssh2，ADR-004 修订拆分）：mod.rs 连接与通道装配、auth.rs 认证信息叠加与连接期询问、pump.rs 多路复用
src/session/transport/local.rs 本地 PTY 传输（默认/用户指定 Shell 与 WSL 发行版的子进程命令都在这里给）
src/session/transport/jump.rs  跳板机回环桥（ADR-020）
src/session/transport/sftp_task.rs SFTP 任务引擎（上传/下载/递归删除状态机 + RemoteFs 抽象）
src/session/transport/forward.rs 端口转发：非阻塞本地/远程监听、SOCKS5 握手、通道↔socket 桥接（ADR-011/038）
src/session/transport/agent_forward.rs agent 转发：libssh2 回调的 FFI 与裸通道包装（ADR-012）
src/session/transport/monitor.rs 远端监控采集：exec 通道脚本、计数器差值（CPU/内存/磁盘/网速）
src/session/transport/poll.rs  SSH 跨平台等待原语：fd+poll+自管道（Unix）/ RawSocket+socketpair（Windows）

── 领域层 ──
src/connect.rs                 SSH 连接表单与列表弹窗：目标解析、认证信息、转发规则、保存计划（ADR-026/048）
src/connections.rs             保存的连接：按条目 id 读写与原子落盘（不含任何口令，ADR-048 修订）
src/settings/                  外观与终端设置：读写、校验、原子落盘；form.rs 是设置表单与设置模态
src/theme/                     主题（ADR-022）：配色方案、内置主题表、UI token 推导与主题选择器（ADR-027）
src/forward_form.rs            运行中新增转发规则的小表单（ADR-038）
src/recorder.rs                全局记录门面（ADR-045）：输出日志与操作记录的统一埋点入口
src/workspace.rs               工作区持久化：窗口几何与本地布局树（ADR-037/039）

── 视图层（消费领域数据画界面，不定义业务规则）──
src/views/                     业务渲染件：消费领域层数据画界面，不定义业务规则
src/views/grid.rs              渲染细节：实测格尺寸、命中测试、run 渲染、侧边栏与浮层
src/views/terminal_view.rs     单会话视图：会话句柄、画面帧、输入与鼠标手势、IME、搜索条（ADR-015）、连接期密码询问卡
src/views/panes.rs             标签内分屏（ADR-014）：二分嵌套布局树、比例与分割条、格区域渲染
src/views/sftp_panel.rs        SFTP 文件面板：目录浏览、操作与确认弹窗、进行中任务区（ADR-005）
src/views/probe_panel.rs       本地进程探针面板（ADR-028）：进程树与 LISTEN 端口列表、详情弹层与结束进程、左缘拖宽
src/views/history.rs           会话留痕的两个全屏模态（ADR-044）：SFTP 操作记录、会话日志开关与外部打开
src/views/shell.rs             外壳框架部件：状态底栏、会话栏、横幅、密钥确认与关闭确认模态

── 外壳 ──
src/main.rs                    入口：开窗、应用菜单（⌘Q 退出）与退出策略
src/app.rs                     应用外壳：左侧会话栏、标签与格的创建/分割/切换/关闭、窗口标题；全体格扇出收成 views() 单点（ADR-007 修订二）

── 探针与测试 ──
src/bin/                       探针二进制：pty_probe（Windows ConPTY e2e）、ssh_sftp_probe（SSH/SFTP 非阻塞探针）
src/tests/                     重量级测试集中地（`#[cfg(test)]`）：真实 PTY/sshd 的端到端与性能验收
src/tests/session_e2e.rs       会话端到端：输入、选区、查询应答、SSH/SFTP 全链路（门控）
src/tests/perf_acceptance.rs   阶段 4 性能与资源验收（门控，跑法见 docs/runbooks/perf-acceptance.md）
```

多会话边界：每个标签持有分屏布局树（`panes`），每格是一个独立的 `TerminalView` 实体，
各自持有 worker、画面帧、焦点与输入法处理器；`app` 只保存标签列表、当前下标与格的映射，
不接触任何终端状态。所以"在 A 会话里输入不影响 B"是由结构保证的，
并由 `sessions_keep_independent_state` 用例覆盖。

边界约定：终端状态只在 worker 内产生与变更，UI 只消费画面帧；`input` 与 `views::grid` 都不持有会话，
`app` 只做两者的转接。**字节级输入编码在 worker**：方向键、Home/End 依赖 `APP_CURSOR`，
焦点上报依赖 `FOCUS_IN_OUT`，鼠标上报依赖 `MOUSE_REPORT_*`/`SGR_MOUSE`，模式只有 worker 持有
（见 ADR-003 增补）。UI 侧只保留快捷键绑定与需要视图几何的换算（像素→格）：
退出、会话、复制/粘贴在 `input::app_key_bindings` 声明为 keymap 绑定，命中即派发 action
到挂载点（应用根元素或会话视图），按键因此不会进入会话视图的编码路径——新增应用级快捷键
不需要改动会话视图。
鼠标手势也是"只上报不判定"：UI 送上按下/移动/松开/滚轮与格坐标，由 worker 决定它变成
程序的上报、本地选区还是滚动。

后续实现必须以需求规格和已接受 ADR 为准；与实现不一致时，应先更新对应文档或新增 ADR，再修改行为。
