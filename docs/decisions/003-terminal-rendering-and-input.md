# ADR-003：终端渲染与输入边界

状态：已接受

## 背景

`alacritty_terminal` 提供 ANSI/VT 解析、终端网格、scrollback、选择状态与 damage 信息，但不提供 GPUI 视图、字体渲染、平台输入法或剪贴板集成。若让 UI 直接持有并修改 `Term`，会与会话 worker 的唯一所有权冲突；若只把原始 PTY 字节交给 UI，解析与渲染会阻塞交互线程。

## 决策

- 会话 worker 唯一持有 `alacritty_terminal::Term` 与 ANSI processor，并在 worker 中消费 PTY 或 SSH 输出。
- worker 使用 `Term::damage` 判断需要刷新的区域，但向 UI 发送的是不可变的、可替换的屏幕快照；UI 不持有 `Term`、parser、PTY 或 SSH channel。
- 首个验证程序将快照降级为纯文本可视内容，以证明 PTY 输出、ANSI 解析和 GPUI 回投链路。正式终端视图必须改为按 cell 读取 `RenderableContent`，保留前景色、背景色、样式、宽字符、组合字符、光标和选择信息。
- GPUI 负责字体 fallback、字形渲染、光标绘制、选择高亮、滚动视口、输入法组合文本和系统剪贴板；VT 状态和 scrollback 仍以 `Term` 为唯一真源。
- 键盘、鼠标、粘贴和 resize 在 UI 层编码为会话命令。worker 按输出时的终端模式处理这些命令，并将最终字节写入 PTY 或 SSH shell channel。
- resize 命令必须同时更新 `Term` 尺寸和底层 PTY/SSH PTY 尺寸。两者由同一个 worker 顺序执行，不能由 UI 各自独立调用。
- 首版不以文本快照作为正式渲染接口。文本快照仅属于验证程序，不能承载颜色、组合字符、选区或鼠标命中。

## 增补：输入编码归属 worker（2026 生效实现）

- UI 只上报**语义按键**（`input::KeyInput`：键名、字符、Ctrl/Alt）与**焦点变化**，不产生终端字节。
- 字节级编码在 worker 内由 `TerminalModel::encode_key` / `encode_focus` 完成，因为它必须读 `TermMode`：
  方向键与 Home/End 在 `APP_CURSOR` 下是 SS3（`ESC O x`）而不是 CSI（`ESC [ x`），
  焦点上报只在 `FOCUS_IN_OUT` 打开时才有字节。
- 理由：模式是终端状态，只有 worker 持有。把模式同步给 UI 需要额外的快照字段，并承担两侧版本错位的风险；
  粘贴编码（`encode_paste`）已按同一边界实现，新输入应与它一致。
- 代价：每次按键多一次跨线程跳转（有界命令队列，实测刷屏期间写入耗时约 2µs）。
- 例外：平台快捷键（⌘C/⌘V、Ctrl+Shift+C/V、会话快捷键 ⌘T/⌘W 与 Ctrl+Shift+T/W、退出 ⌘Q 与
  Ctrl+Shift+Q）与滚轮仍由 UI 判定——前者是应用或剪贴板动作，后者需要视图几何（格高）。
  会话快捷键非 macOS 要求 Shift：单独 Ctrl+T/W/Q 是控制字符 ^T/^W/^Q（^W 删词、^T 对调字符、
  ^Q 恢复输出），必须留给前台程序，Ctrl+Shift+T/W 也是 Windows Terminal / GNOME Terminal /
  Konsole 的通行键位；浏览器式统一 Ctrl+T/W 的方案被否，因为那会让 shell 失去 ^W/^T。
- 平台快捷键以 **keymap 绑定 + action 派发**实现（`input::app_key_bindings` 是唯一事实来源，
  键位、平台修饰键约定与生效上下文都在那里）：应用级动作（退出、会话）不设上下文，
  处理器挂在应用根元素或注册为全局监听；复制/粘贴绑定带 "Terminal" 上下文，只在会话
  视图根元素持有焦点时匹配，处理器挂在会话视图上。
- 该结构替代了早期的"视图内谓词判定 + 放行黑名单"：keymap 命中的按键会被 action 消费，
  不再进入会话视图的 `on_key_down` 编码路径，因此会话视图不需要知道任何应用级快捷键，
  新增快捷键不改动它。复制/粘贴的 "Terminal" 上下文是动态的（`focus.is_focused(window)`）：
  SFTP 面板与表单输入框获得焦点后上下文不成立，⌘C/⌘V 仍归它们自己的输入处理。
- 仍未实现、需要时同样放在 worker：鼠标上报（`MOUSE_REPORT_*`、`SGR_MOUSE`、`UTF8_MOUSE`）、
  `APP_KEYPAD`、kitty 键盘协议（当前 `Config::kitty_keyboard` 为 false，核心会向程序回答不支持，行为自洽）。

## 后果

- 终端协议状态与 IO 都留在后台，GPUI 主线程只处理可丢弃的渲染数据。
- 终端视图可在不改变 SSH/PTY worker 的情况下独立演进。
- 正式渲染器需实现 cell 到 GPUI 文本 run 的映射，以及 damage 与光标/选区失效区域的合并；这部分不能由 `alacritty_terminal` 自动完成。
- 输入编码需要在实现时以单元测试覆盖普通文本、修饰键、应用光标模式、bracketed paste、鼠标协议和 IME 提交文本。

## 验证

- 本地终端验证程序启动 Shell，将 PTY 输出交给 ANSI parser，并在 GPUI 窗口显示由 `Term` 生成的快照。
- ANSI 彩色输出在快照中不含转义序列，证明解析发生在 worker 而非 UI。
- worker 退出、PTY EOF 和 UI 接收端关闭都产生可观察终态。
- 正式视图实现前，增加 CJK 宽字符、组合字符、alternate screen、scrollback、resize 与复制/粘贴的独立测试。

## 修订（2026-10）

「仍未实现」中的鼠标上报（1000/1002/1003，legacy/UTF-8/SGR 三种编码）与 kitty
键盘协议（ADR-031，`kitty_keyboard` 已开启）均已落地。`APP_KEYPAD` 仍受 GPUI
平台能力限制（不区分小键盘按键），保持未实现。
