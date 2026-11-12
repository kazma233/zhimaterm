# 开发指南

构建、运行、测试与代码结构（自 README 移入）。README 只保留产品功能介绍；
功能行为细节见 [features.md](features.md)，模块边界的决策依据见
[ADR-007](../decisions/007-module-boundaries.md)。

## 构建与运行

- 开发运行：`cargo run --bin zhimaterm`——启动可交互的本地终端（左侧栏多会话，支持的全部交互见功能行为说明）。
- 安装到当前用户：`task install`（先构建 release，再按平台做桌面集成：Linux 装进 `~/.local`、macOS 组装 .app 到 `~/Applications`、Windows 注册开始菜单）；卸载 `task uninstall`。
- `cargo run -p zhimaterm-session --bin zhimaterm-ssh-sftp-probe`：验证单条 SSH session 中非阻塞 Shell 与 SFTP 的交替推进。该命令只使用 SSH Agent，且要求显式设置 `ZHIMATERM_SSH_ADDR`、`ZHIMATERM_SSH_USER` 和 `ZHIMATERM_SSH_HOST_KEY_SHA256`。不会读取、保存或打印密码、私钥或 `known_hosts`。
- 构建依赖：ghostty 终端后端（[ADR-051](../decisions/051-ghostty-vt-backend.md)）经仓内
  `ffi/ghostty-vt-sys`（workspace 成员）用 Zig 编译 vendored 的 libghostty-vt，需要 Zig 0.17.x
  （PATH 或 `ZIG` 环境变量，自行安装）；首次构建联网下载 `GHOSTTY.lock` 固定的源码包
  （sha256 校验），离线构建设 `GHOSTTY_SOURCE_DIR` 指向本地 Ghostty 源码树。FFI 绑定
  预生成提交，构建只需要 Zig；再生成与更新 pin 见
  [runbooks/ghostty-bindings](../runbooks/ghostty-bindings.md)。该后端目前只在 macOS 实测过运行时。

## 依赖选择与构建约束

manifest 保持纯声明，不写选型说明；新增或更换依赖时同步本节。依赖方向本身
由 Cargo 依赖图强制（见上节“模块划分”与 ADR-007 修订八）。

### 版本与工具链

- 全仓 `rust-version = "1.98"`：gpui-pre 0.3.10 的编译期下限，本仓不抬高。
- 开发工具链由 `rust-toolchain.toml` 固定为具体版本（`1.99.1`），不用 `stable`：
  stable 会随上游漂移，本地、CI 与发布产物不再是同一行为基线。换版本时先在
  本机验证 `cargo test` 全绿。工具链决定“用什么构建”，`rust-version` 决定
  “最低能用什么构建”，两者职责不同。
- 需要可复现的 crate 用精确 pin：`gpui-pre =0.3.10`、`serde =1.0.229`、
  `serde_json =1.0.151`、`ssh2 =0.9.6`、`libssh2-sys =0.3.3`、
  `windows-sys =0.61.2`、`windows-registry =0.6.1`。
- `[profile.dev.package."*"]` 把依赖开到 opt-level 2：debug 下 gpui 的
  DirectWrite 整形包装等不优化时，cat 满屏刷单帧约 100ms，UI 线程被占满；
  应用代码保持可调试。

### 各 crate 的依赖归属与理由

- `zhimaterm-base`
  - `gpui`：theme 的进程级 Global 与 ui 组件；ui 单测经 dev-dependency 开
    `test-support`（release 不带）。
  - `anyhow`：配置路径的错误链；`smol`：`wake::WakingSender` 的通道；
    `unicode-segmentation`：ui 的文本边界处理。
  - Unix `libc`：唤醒管道的自管道。
- `zhimaterm-terminal`
  - `alacritty_terminal`：VT 状态与序列解析（ADR-001）。
  - `termwiz`：流式解析器后端（ADR-050）；屏幕模型仍在本仓
    （`backend/termwiz.rs`），换后端不改变终端语义。
  - `ghostty-vt-sys`：vendored libghostty-vt 的仓内薄绑定（ADR-051）。构建需要
    Zig 0.17.x（PATH 或 `ZIG`），首次联网下载 `GHOSTTY.lock` 固定的源码包
    （sha256 校验），离线构建用 `GHOSTTY_SOURCE_DIR`；绑定预生成提交，日常
    构建不需要 bindgen/libclang。
  - `regex`：搜索；`serde`/`serde_json`：转录格式；`smol`：回放与异步等待；
    `unicode-width`：组合文本按 wcwidth 折算占格。
- `zhimaterm-local`
  - `gpui`：本地探针面板；`anyhow`：错误链；`smol`：探针的事件通道。
  - Unix `libc`：PTY 与 cwd 采集。
  - Windows `windows-registry`：WSL 发行版枚举（ADR-021）；`windows-sys` 的
    Console/ToolHelp/PEB 等 feature：ConPTY 调用、进程探针，同时服务
    `pty_probe` 探针二进制（`src/bin/`）。
  - dev `gpui`（`test-support`）。
- `zhimaterm-recording`
  - `gpui`：留痕模态视图与 `EntityId`；`anyhow`：错误链；`chrono`：输出
    日志时间戳与任务的结束时刻。
  - dev `gpui`（`test-support`）：记录器单测。
- `zhimaterm-session`
  - `gpui`：`SessionConfig::pane` 的实体 id。
  - `ssh2`（`vendored-openssl`、`openssl-on-win32`）与 `libssh2-sys`：SSH
    传输（ADR-004）；与探针同一版本。
  - `smol`：通道与等待；`anyhow`：错误链；`chrono`/`serde_json`：回滚格式。
  - Unix `libc`：`poll.rs` 的多路等待；`socket2`：监听 socket 在 bind 前设
    `SO_REUSEADDR`（ADR-038，运行中删规则后同端口重建不被 TIME_WAIT 挡住；
    Windows 不设，避免端口劫持）。
  - `src/bin/ssh_sftp_probe.rs` 探针归本 crate，按 `#[path]` 复用
    `poll.rs` 并直接链接 `ssh2`/`libssh2-sys`。
  - dev `gpui`（`test-support`）。
- `zhimaterm-connections`
  - `keyring`（`apple-native`、`windows-native`、`sync-secret-service`）：
    OS 安全凭据存储；`uuid` v4：保存连接的条目 id（ADR-018 修订，凭据按 id
    挂钥匙串，条目身份不参与）。
  - `serde`/`serde_json`：`connections.json`；`anyhow`：错误链；`gpui`：表单
    与模态渲染。
  - dev `gpui`（`test-support`）：连接表单的实体级单测。
- app（`zhimaterm-app`）
  - `gpui` + `gpui_platform`（`font-kit`、`runtime_shaders`、`wayland`、`x11`）：
    桌面 UI 与窗口平台；`serde`/`serde_json`：设置与工作区；`anyhow`：
    错误链；`chrono`/`regex`/`unicode-segmentation`/`unicode-width`：视图层
    的时间、搜索与文本边界。
  - macOS `objc2` 三件套：窗口几何恢复时用 AppKit 类方法实测标准标题栏高度
    （外框→内容区换算）。
  - build-dependency `winresource`：Windows 资源段（图标）。
  - dev `gpui`（`test-support`）：设置表单的实体级单测。
- `ffi/ghostty-vt-sys`
  - `links = "ghostty-vt"`：全仓唯一拥有 libghostty-vt 的构建与链接（ADR-051）。
  - build-dependencies `flate2`/`sha2`/`tar`：解包与校验 pin 源码包。
  - `ureq` 3 走系统 TLS 栈（macOS/Windows `NativeTls`，其余 `rustls` +
    `platform-verifier`）：不给构建机引入 OpenSSL 依赖，根证书用平台系统根。
  - `bindgen`/`heck` 是 `bindgen-tool` feature 的可选依赖：只在更新 pin 或
    头文件口径时手动跑 `gen-bindings`，见
    [runbooks/ghostty-bindings](../runbooks/ghostty-bindings.md)。

## 测试与验证

- **收尾检查（改了代码就跑）**：`cargo fmt` → `cargo clippy --workspace --all-targets` →
  `cargo test`，三项全绿才算改完。clippy 对本仓代码要求 0 warning；`--all-targets` 不能省，
  否则 `#[cfg(test)]` 里的代码不参与编译与 lint。改了 `#[cfg]` 分支的代码，还需在对应
  平台上过一遍 `cargo check --all-targets`。
- 测试分层：小型单元测试随模块内联（`#[cfg(test)] mod tests`，不需要外部环境，`cargo test` 直接跑）；
  **依赖真实 PTY/sshd 的重量级用例集中在 [`crates/session/src/tests/`](../../crates/session/src/tests/)**，全部 `#[ignore]` 门控：
  - 会话端到端：`cargo test -p zhimaterm-session --lib "tests::session_e2e::ssh" -- --ignored --test-threads=1`（先按 [runbooks/ssh-e2e](../runbooks/ssh-e2e.md) 起 sshd）。
  - WSL 会话端到端：`set ZHIMATERM_WSL_DISTRO=<发行版名> && cargo test wsl_session -- --ignored`（验证进入指定发行版、落在 Linux home、退出即收，ADR-021）。
  - 性能验收：见 [runbooks/perf-acceptance](../runbooks/perf-acceptance.md)（必须 `--release`）。
- 录制流回归：`crates/terminal/src/replay.rs` 按真实读取节奏回放字节流，检查我们自己那层的不变量（快照的段宽自洽、连续快照一致、回看不越界、选区可复制）。默认跑文件内的确定性语料；外部录制（例如 `alacritty_terminal` 自带的 `tests/ref/*/alacritty.recording`）用：

  ```
  ZHIMATERM_RECORDING_DIR=<目录> cargo test -p zhimaterm-terminal --lib replays_external_recordings -- --ignored --nocapture
  ```

  实测：45 段真实录制（956 KB）全部通过，用时约 168 ms。

## 模块划分

见 ADR-007（2026-11-12 修订十一）：强制的分层边界落在 workspace crate，app crate 只装
外壳与视图；根 Cargo.toml 是纯 workspace（无根级 src/）。依赖只允许自上而下，跨 crate 的反向引用由 Cargo 依赖图直接拒绝。

### crate 总览

```
crates/base         zhimaterm-base         叶子与基础层
crates/terminal     zhimaterm-terminal     终端模型（零 gpui 直接引用）
crates/local        zhimaterm-local        本地终端域（PTY/Shell/WSL/探针）
crates/recording    zhimaterm-recording    会话留痕域
crates/session      zhimaterm-session      会话域：内核、传输（SSH 协议/SFTP）、重量级测试与探针
crates/connections  zhimaterm-connections  SSH 连接管理域（不实现 SSH 协议）
crates/app          zhimaterm-app          外壳与视图（workspace 唯一 bin，bin 名 zhimaterm）
```

依赖边：`app → connections → session → terminal → base`；`session → local`
与 `session → recording`（两者只依赖 base，机制保持叶子地位）；app 另可用
全部 crate。SSH 协议本体（libssh2 会话、认证、pump）在
`zhimaterm-session` 的 `transport::ssh`，连接的表单/保存/钥匙串在
`zhimaterm-connections`。

### 基础层（`crates/base/src/`）

```
config_paths.rs            应用配置目录（主机密钥记录与保存的连接都在这里）
font.rs                    字体领域：等宽候选实测、回退链、格尺寸测量（CellMetrics）与默认字号
input.rs                   输入提取与语义类型：KeyInput、鼠标手势、快捷键绑定（keymap）与滚轮判定
wake.rs                    唤醒管道：WakeupHandle/WakeupRead/wakeup_pair 与 WakingSender；
                           std+libc 自包含，SFTP 与会话传输共用（断开 sftp → session 反向边）
theme/                     配色方案、内置主题表、UI token 推导与进程级主题状态（ADR-022）；
                           只依赖 gpui，不引用 ui/views/session
ui/                        自己写的 UI 组件（与终端无关，可被别处直接复用）
ui/input/                  文本输入组件：调色板、表单行助手（field）与模块组装；core.rs 编辑核心，ime.rs IME 状态机
ui/overlay.rs              统一浮层栈：全窗口模态唯一归属，栈序即 z 序（ADR-043/044）
ui/dialog.rs               通用确认/输入/密码对话框（ADR-044/048）
ui/card.rs / panel.rs / menu.rs / select.rs / dock.rs   卡片四区、模态遮罩与按钮、右键菜单、收起式下拉、右缘停靠面板外壳
ui/canvas.rs               组件底座：鼠标监听、输入法处理器、bounds 记录
```

### 终端模型（`crates/terminal/src/`）

```
lib.rs                     终端模型：VT 状态、滚动、选区、run 快照
backend/                   终端核心后端：中性契约（TerminalCore trait）与各实现（ADR-050/051）
selection.rs               选区语义：词/行/块选与复制文本（共享层，后端无关）
search.rs                  搜索语义：逐行正则、智能大小写、方向导航（共享层，后端无关）
contract.rs                后端契约测试：同一行为场景跑全部后端（含 resize 差分）
transcript.rs              行转录的格式编解码与逻辑行装配（输出日志与回滚共用）
colors.rs                  VT 颜色 → RGB 与颜色查询
input.rs                   输入编码：按键/粘贴/焦点/鼠标手势 → 终端字节（kitty/xterm 口径）
replay.rs                  录制流回归
```

### 本地终端域（`crates/local/src/`）

```
shells.rs                  系统 Shell 候选列表（/etc/shells 或 PATH 探测，ADR-036）
pty/                       自研本地 PTY（ADR-025）：ConPTY/openpty、MSVCRT 引用与退出唤醒
cwd.rs                     本地 shell 工作目录采集（Linux /proc、macOS lsof，ADR-039）
peb.rs                     Windows 跨进程读 PEB：他人 cwd 与完整 argv（#[cfg(windows)]）
wsl.rs                     WSL 发行版枚举与启动命令（ADR-021）
probe/                     本地进程探针（ADR-028）：三平台采集与结束进程、树构建/过滤/端口合并
probe_panel.rs             本地进程探针面板（ADR-028）
bin/pty_probe.rs           Windows ConPTY e2e 探针（ADR-025）
```

### 会话留痕域（`crates/recording/src/`）

```
output_log.rs              会话输出日志：文件分配、首尾标记与故障停写（ADR-017）
recorder.rs                全局记录门面（ADR-045），worker 与视图都只依赖它（叶子地位）
history.rs                 任务/操作的数据模型与两个全屏模态（ADR-044）；SFTP 任务的
                           进度/终态由写入方折算成本域投影类型，记录开关经 LoggingTarget 注入
```

### 会话域（`crates/session/src/`）

```
lib.rs                     会话协议：命令/事件、目标（本地/SSH）、句柄
worker.rs                  会话 worker：与后端无关的循环、终端模型与回收
reconnect.rs               重连计划（ADR-013 修订）的纯状态机
scrollback.rs              本地格回滚持久化：版本化 JSONL 与旧格式回退、文件命名契约（ADR-039/040）
sftp.rs                    SFTP 契约：SftpHandle 命令句柄、任务事件流、远端条目模型
transport/                 传输后端：本地 PTY、SSH 多路复用 pump、跳板桥、端口转发、agent 转发、远端监控
transport/ssh/             SSH 传输（libssh2，ADR-004 修订拆分）：连接装配、认证、pump
transport/local.rs         本地 PTY 传输（默认/用户指定 Shell 与 WSL 发行版的子进程命令）
transport/jump.rs          跳板机回环桥（ADR-020）
transport/sftp_task.rs     SFTP 任务引擎（上传/下载/递归删除状态机 + RemoteFs 抽象）
transport/forward.rs       端口转发：非阻塞监听、SOCKS5 握手、通道↔socket 桥接（ADR-011/038）
transport/agent_forward.rs agent 转发：libssh2 回调的 FFI 与裸通道包装（ADR-012）
transport/monitor.rs       远端监控采集：exec 通道脚本、计数器差值（CPU/内存/磁盘/网速）
transport/poll.rs          SSH 跨平台等待原语：fd+poll（Unix）/ RawSocket+WSAPoll（Windows）；
                           保持 std+libc 自包含，探针二进制按 `#[path]` 复用
tests/support.rs           会话测试共享驱动层：spawn/wait/type_line 的唯一实现
tests/session_e2e.rs       会话端到端：输入、选区、查询应答、SSH/SFTP 全链路（门控）
tests/perf_acceptance.rs   阶段 4 性能与资源验收（门控，见 runbooks/perf-acceptance.md）
bin/ssh_sftp_probe.rs      SSH/SFTP 非阻塞探针（诊断工具，按 `#[path]` 复用 poll.rs）
```

### SSH 连接管理域（`crates/connections/src/`）

```
connect/                   SSH 连接表单与列表弹窗（ADR-026/048），按变更轴拆子模块（ADR-007 修订四）：
  mod.rs                   表单状态机与共享类型（ConnectForm、开关/段枚举、Field、FormError）
  parse.rs                 文本解析：端口、目标串 `[user@]host[:port]`、字段合并、`~` 展开
  target.rs                保存的连接 → 会话目标（列表直连路径）
  auth.rs                  认证装配（form_auth/form_jump）与保存计划 SavePlan
  forward.rs               转发规则草稿、编辑动作与规则行渲染（ADR-011）
  view.rs                  连接弹窗：列表/编辑两视图渲染与 ModalView 接入
  tests.rs                 单元测试与夹具
connections.rs             保存的连接：按条目 id 读写与原子落盘（不含口令，ADR-048 修订）
keychain.rs                OS 安全凭据存储：凭据按条目 id 挂钥匙串（ADR-018 修订）
forward_form.rs            运行中新增转发规则的小表单（ADR-038）；规则落点经 ForwardTarget
                           能力接口注入，域不依赖会话视图类型（ADR-007 修订六）
lib.rs                     crate 根：模块声明与对外 re-export
```

### 外壳与视图（`crates/app/src/`）

```
settings/                  外观与终端设置：读写、校验、原子落盘；form.rs 是设置表单与设置模态
workspace.rs               工作区持久化：窗口几何与本地布局树（ADR-037/039）
views/                     跨业务的共享渲染件（消费领域数据画界面，不定义业务规则）
views/theme_picker.rs      主题选择器（ADR-027）：侧栏入口行与向上弹出的锚定浮层
views/grid.rs              渲染细节：实测格尺寸、命中测试、run 渲染、侧边栏与浮层
views/terminal_view.rs     单会话视图：会话句柄、画面帧、输入与鼠标手势、IME、搜索条（ADR-015）
views/panes.rs             标签内分屏（ADR-014）：二分嵌套布局树、比例与分割条、格区域渲染
views/sftp_panel.rs        SFTP 文件面板：目录浏览、操作与确认弹窗、进行中任务区（ADR-005）
views/session_state.rs     会话视图归约：控制事件 × 视图状态 → 效果清单（纯数据与纯决策）
views/terminal_keys.rs     会话视图的按键裁决：keydown 守卫链与死会话裸键的纯决策
views/shell.rs             外壳框架部件：状态底栏、会话栏、横幅、密钥确认与关闭确认模态
app/                       应用外壳（ADR-007 修订八按变更轴拆目录）：
  mod.rs                   状态结构、构造、Tab/拖拽类型、浮层薄转发与共享小工具
  sessions.rs              会话/标签/格的创建、分割、激活、关闭、回收与拖拽排序
  workspace_io.rs          工作区快照、恢复、防抖落盘与孤儿回滚文件清理
  modals.rs                连接/设置/转发/快捷键/重命名/口令对话框与动作回调装配
  keys.rs                  裸键分发与侧栏菜单的捕获阶段按键
  render.rs                Render 实现、侧栏行/入口、格区域与锚定浮层渲染装配
main.rs                    入口：开窗、应用菜单（⌘Q 退出）与退出策略
```

多会话边界：每个标签持有分屏布局树（`panes`），每格是一个独立的 `TerminalView` 实体，
各自持有 worker、画面帧、焦点与输入法处理器；`app` 只保存标签列表、当前下标与格的映射，
不接触任何终端状态。所以"在 A 会话里输入不影响 B"是由结构保证的，
并由 `sessions_keep_independent_state` 用例覆盖。

边界约定：终端状态只在 worker 内产生与变更，UI 只消费画面帧；`zhimaterm_base::input`
与 `views::grid` 都不持有会话，`app` 只做两者的转接。**字节级输入编码在 worker**：方向键、Home/End 依赖 `APP_CURSOR`，
焦点上报依赖 `FOCUS_IN_OUT`，鼠标上报依赖 `MOUSE_REPORT_*`/`SGR_MOUSE`，模式只有 worker 持有
（见 ADR-003 增补）。UI 侧只保留快捷键绑定与需要视图几何的换算（像素→格）：
退出、会话、复制/粘贴在 `zhimaterm_base::input::app_key_bindings` 声明为 keymap 绑定，命中即派发 action
到挂载点（应用根元素或会话视图），按键因此不会进入会话视图的编码路径——新增应用级快捷键
不需要改动会话视图。
鼠标手势也是"只上报不判定"：UI 送上按下/移动/松开/滚轮与格坐标，由 worker 决定它变成
程序的上报、本地选区还是滚动。

后续实现必须以需求规格和已接受 ADR 为准；与实现不一致时，应先更新对应文档或新增 ADR，再修改行为。
