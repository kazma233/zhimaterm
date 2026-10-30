# 文档目录

分层约定：

- `context/`：需求口径、排障记录、联调约束、临时决策。
- `decisions/`：ADR——阶段性技术决策与方案取舍；被取代的机制在文内修订段标注，不改写历史。
- `runbooks/`：可复用的排查、验证、清理步骤。
- `archive/`：已失效或已替代的文档，仅作历史依据，归档原因见各文件头部注记。

根目录另有 [GLOSSARY.md](../GLOSSARY.md)：领域术语表（ADR 与代码注释以「见 GLOSSARY」引用）。

## 核心文档

- [需求规格](context/requirements.md)：产品边界、验收范围和性能目标。
- [功能行为说明](context/features.md)：各功能的实际行为与交互细节（多会话、分屏、搜索、SSH/SFTP、主题、日志、鼠标与输入法、性能设计等，README 的行为章节移此）。
- [开发指南](context/development.md)：构建、运行、安装、测试与验证、模块划分（README 的开发章节移此）。
- [聚合待办](context/todo.md)：功能池、人工验收、平台验证、技术债与明确不做的唯一入口。
- [平台验证现状](context/platform-verification.md)：三平台构建与实机验证记录。
- [MobaXterm 差距与优先级](context/mobaxterm-gap-and-priorities.md)：完整能力对比与 P2/P3 排序依据。
- [待人工验收清单](context/manual-acceptance.md)：合成输入进不了 gpui 窗口、需真人过一遍的操作步骤（全空后删该文件）。

## 决策记录（`decisions/`，按编号）

- [ADR-001：GPUI 与会话运行时边界](decisions/001-runtime-and-session-ownership.md)：worker 所有权、SSH 非阻塞调度与事件语义。
- [ADR-002：SSH 信任、凭据与 SFTP 文件安全](decisions/002-ssh-and-sftp-safety.md)：主机密钥、凭据、覆盖与临时文件规则。
- [ADR-003：终端渲染与输入边界](decisions/003-terminal-rendering-and-input.md)：VT 状态、GPUI 视图、输入和 resize 的职责划分。
- [ADR-004：SSH 传输接缝与 libssh2 单线程 pump](decisions/004-ssh-session-transport.md)：传输接缝、pump 调度与连接期确认。
- [ADR-005：SFTP 子系统与 SSH 会话生命周期解耦](decisions/005-sftp-subsystem-and-session-lifecycle.md)：pump 多路复用、shell 退出后会话保留、SFTP 命令契约。
- [ADR-006：Windows 构建的 OpenSSL 后端与 Strawberry Perl](decisions/006-windows-openssl-build.md)：vendored OpenSSL 的 Perl 依赖与构建路径。
- [ADR-007：模块边界与依赖方向](decisions/007-module-boundaries.md)：模块分层、依赖方向与装配职责。
- [ADR-008：终端网格的立即模式渲染与字形回钉](decisions/008-terminal-grid-immediate-rendering.md)：逐帧网格绘制、字形定位与 resize 行为。
- [ADR-009：文本输入组件](decisions/009-text-input.md)：编辑核心分层、掩码的字节对齐约定、粘贴清洗与单行回车放行。
- [ADR-010：SSH 认证信息叠加与 agent 参与](decisions/010-auth-credentials-stack.md)：素材集合与尝试顺序、旧配置迁移、agent 非阻塞重试的硬约束。
- [ADR-011：SSH 端口转发（本地与远程）](decisions/011-port-forwarding.md)：规则语义、pump 桥接模型与状态上报。
- [ADR-012：agent 转发的 FFI 桥接边界](decisions/012-agent-forwarding-ffi.md)：libssh2 回调与最小 FFI、请求时机、Windows 现状。
- [ADR-013：keepalive 心跳与断线自动重连](decisions/013-keepalive-and-reconnect.md)：判活标准、退避参数与资格条件；重连计划（ReconnectPlanner）单点。
- [ADR-014：终端分屏](decisions/014-split-panes.md)：标签内的二分嵌套、新格会话类型、比例与键位（布局持久化见 ADR-037）。
- [ADR-015：终端内搜索](decisions/015-terminal-search.md)：worker 侧搜索、单行匹配边界、搜索条与高亮。
- [ADR-016：本地格不设底栏](decisions/016-local-pane-without-status-bar.md)：启动与故障信息进网格中央；默认 Shell 格的探针底栏见 ADR-028。
- [ADR-017：会话输出日志](decisions/017-session-output-log.md)：只记输出、会话内运行时开关（底栏「记录」模态）、行转录与失败语义。
- [ADR-018：记住密码](decisions/018-remember-password.md)：OS 安全凭据存储、按条目 id 的条目键与删除时机、连接期临时密码询问。
- [ADR-019：SSH 会话向远端发送 LANG](decisions/019-send-client-locale-env.md)：下发策略、AcceptEnv 结果与底栏角标。
- [ADR-020：跳板机/ProxyJump（单级）](decisions/020-jump-host.md)：回环桥、两次主机确认与凭据边界。
- [ADR-021：WSL 会话是本地 Shell 的一种](decisions/021-wsl-local-shell.md)：发行版枚举口径、入口交互与本地格语义。
- [ADR-022：主题系统](decisions/022-theme-system.md)：配色方案与 UI token 的推导模型、内嵌主题表与切换交互。
- [ADR-023：右键上下文菜单](decisions/023-context-menus.md)：右键留给本地菜单不转发前台程序、SFTP 条目操作入口。
- [ADR-024：按钮基础组件与点击语义](decisions/024-button-click-semantics.md)：松开在按钮内才触发、命中区即按钮盒子。
- [ADR-025：自研本地 PTY 替换 portable-pty](decisions/025-selfhost-pty.md)：`src/pty` 的语义基线、与 portable-pty 0.9.0 的差异和验证记录。
- [ADR-026：连接表单深化](decisions/026-connect-form-deepening.md)：自持建造与类型化出口（编辑态基线已按 ADR-048 移除）。
- [ADR-027：侧栏浮层与选择器类 UI 的归属](decisions/027-popover-species-ownership.md)：锚定浮层、主题选择器与实体化边界。
- [ADR-028：本地进程探针与右侧边栏](decisions/028-local-probe-sidebar.md)：本地默认 Shell 格的进程树与 LISTEN 端口采集、三平台口径与面板生命周期。
- [ADR-029：跨平台代码的模式档位与抽象时机](decisions/029-cross-platform-code-conventions.md)：cfg 分叉、平台抽象与验证要求。
- [ADR-030：macOS 打包](decisions/030-macos-local-network-and-codesign.md)：本地网络权限声明与稳定签名身份。
- [ADR-031：按键修饰键编码](decisions/031-kitty-keyboard-and-modifier-encoding.md)：xterm 修饰参数与 kitty 键盘协议。
- [ADR-032：终端保真补齐](decisions/032-terminal-fidelity-gaps.md)：F 键编码、文字属性进快照与两条绘制路径、备用屏滚轮、同步输出超时、光标形状与闪烁、COLORTERM。
- [ADR-033：⌥ 作为 Alt](decisions/033-option-as-alt.md)：option 修饰的语义与中文输入。
- [ADR-034：块元素按格子几何绘制](decisions/034-block-elements-as-geometry.md)：块元素的几何绘制路径。
- [ADR-035：滚动历史上限设置](decisions/035-scrolling-history-setting.md)：0–100,000 行、即时生效与 alacritty Config 单点构造。
- [ADR-036：默认本地 Shell 与工作目录](decisions/036-default-local-shell.md)：系统候选下拉、登录 Shell 语义、cwd 穿透 pty 与「不做初始尺寸」取舍。
- [ADR-037：窗口几何与本地布局持久化](decisions/037-workspace-persistence.md)：workspace.json、SSH 格裁剪、本地自动启动与屏外回退。
- [ADR-038：动态 SOCKS 转发与运行中增删](decisions/038-dynamic-socks-and-runtime-forwards.md)：稳定规则 id、SOCKS5 握手与应答顺序、非阻塞监听、浮层增删与回写。
- [ADR-039：本地格重启恢复](decisions/039-restore-cwd-and-scrollback.md)：宿主侧 cwd 采集、回滚文件与分界行、容量/失败边界。
- [ADR-040：本地格回滚全彩恢复](decisions/040-full-color-scrollback.md)：样式 run 转录、语义色与真彩色、版本化 JSONL、旧格式回退。
- [ADR-041：侧栏底部动作行与主题色板入口](decisions/041-sidebar-actions-and-theme-swatch.md)：会话入口单行、设置移至顶栏、主题入口只留色板。
- [ADR-042：会话显示编号在空列表时重置](decisions/042-display-number-reset.md)：显示编号与回滚键分离、最后一个会话关闭后从 1 重新开始。
- [ADR-043：弹窗焦点作用域](decisions/043-modal-focus-scope.md)：弹窗外壳是焦点归属层——打开不落输入框、点空白回到无焦点态、Tab 焦点环、关闭归还焦点。
- [ADR-044：统一浮层栈与 ModalView 接入规范](decisions/044-overlay-stack-and-modal-view.md)：OverlayHost 栈是浮层唯一归属（同物种互斥替换、跨物种叠放），业务模态实现 trait 任意视图一行呼出；通用 Dialog；操作记录与「记录」模态入口收到底栏。
- [ADR-045：全局记录门面（recorder）](decisions/045-global-recorder-facade.md)：输出日志与操作记录的统一埋点入口，注册表按格注册/注销，句柄不再层层穿透。
- [ADR-046：会话在尺寸就绪后才开工](decisions/046-startup-size-gate.md)：worker 等 UI 首个 Resize（250ms 兜底）再开 PTY、建模型、灌回滚；恢复内容不被首次 resize 吸回视口。
- [ADR-047：侧栏会话行手动重命名](decisions/047-sidebar-session-rename.md)：名字挂标签不挂格、右键菜单入口与 Esc 捕获、通用 Dialog prompt 首个消费方、随工作区落盘。
- [ADR-048：连接弹窗列表优先、编辑与连接分离、连接名称](decisions/048-connect-modal-list-first-and-names.md)：列表行上「连接」「编辑」「删除」显式按钮、「编辑」才进表单、主按钮不再随改动跳文案、仅密码条目叠密码框、校验错误随编辑即时消长（单字段贴输入框、组合结论与钥匙串失败进底栏）、连接名随条目落盘并成为标签初始名。
- [ADR-049：回滚恢复的尺寸信任与不变式守护](decisions/049-scrollback-restore-size-gate.md)：修订 ADR-046 的恢复口径，超时兜底尺寸此后只用于首次 PTY 建立、不再用于回滚重建，恢复格按落盘尺寸回放。

## 背景与调研（`context/`）

- [ssh-agent 图解](context/ssh-agent-explained.md)：面向不熟悉 SSH 内部的读者，讲清 agent 与转发在做什么。
- [SSH 转发与跳板机入门](context/ssh-forward-vs-jump-host-primer.md)：本地/远程/动态转发的区别与跳板机选型。
- [中文拼音输入排障](context/ime-pinyin-troubleshooting.md)：三层叠加假象的排查路径与可复用诊断方法（ADR-033）。

## 运行手册（`runbooks/`）

- [性能验收](runbooks/perf-acceptance.md)：固定输入、目标环境、结果记录与人工交互清单。
- [SSH 端到端测试](runbooks/ssh-e2e.md)：门控 sshd 环境的三种形态、跑法与用例覆盖表。

## 归档（`archive/`，仅作历史依据）

- [终端完备性盘点](archive/terminal-completeness-audit.md)：6 项自研层缺口的证据与排序依据（已由 ADR-032 落地）。
- [portable-pty 采用边界](archive/portable-pty-maintenance.md)：被自研 PTY 替换前的选型记录（ADR-025）。
- [Alacritty 可借鉴项](archive/alacritty-lessons.md)：调研结论已全部落地或定型（kitty 键盘协议随 ADR-031 落地）；唯一未做项 APP_KEYPAD 已移入聚合待办功能池；damage 增量快照的否定结论体现在 ADR-003。
- [tokio 迁移调研](archive/tokio-migration-research.md)：结论（不整体迁移、未来边界共存）由需求规格 §2 承接；事实段为 2026-10 快照。
- [SSH 转发与跳板机 e2e 验证记录（WSL tester）](archive/ssh-e2e-wsl-tester-verification.md)：2026-10-24 一次性验证记录；三种环境形态已并入 `runbooks/ssh-e2e.md`，复现脚本转正于 `scripts/`。
- [终端项目与 VT/PTY 库选型参考](archive/Ref.md)：立项期调研；技术栈已定型（gpui + alacritty_terminal + 自研 pty + ssh2），「portable-pty 首选」等推荐已被 ADR-025 替代。
