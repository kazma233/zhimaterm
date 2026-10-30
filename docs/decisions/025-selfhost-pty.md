# ADR-025:自研本地 PTY 替换 portable-pty

状态:已接受(2026-10)

## 背景

`portable-pty 0.9.0`(2025-02)之后 crates.io 再无发布,修复只进 wezterm main
不跟版;它把 14 个传递依赖拉进构建树(winapi、winreg 0.10、serial2、
shared_library、shell-words、downcast-rs、filedescriptor、bitflags 1.x、
lazy_static、log、nix 等多为沉寂的旧库)。对照 wezterm main(2026-10 实测
diff):0.9.0 之后仅有 Windows `kill()` 返回值判断反转的修复(zhimaterm 走
`Child::kill()` 且忽略返回值,不受影响)、winapi→windows-sys 迁移与拼写修正,
unix.rs 零改动——0.9.0 的语义即当前最新语义。生态里也没有同时满足
「跨平台含 Windows ConPTY + 不绑定异步运行时」的活跃替代。

zhimaterm 的实际使用面只有 `transport::local` 与 `wsl` 两处、约十个 API:
`openpty`、`PtySize`、`CommandBuilder`、`spawn_command`、`try_clone_reader`、
`take_writer`、`resize`、`try_wait`/`kill`/`wait`、`as_raw_handle`;运行时
模型是阻塞 `Read`/`Write` + 会话专属 worker,对 PTY 层没有异步要求。

## 决策

新增 `src/pty/`(`mod.rs`/`win.rs`/`unix.rs`),在 MIT 许可下按 portable-pty
0.9.0 的语义移植所需子集,吸收 main 分支的 kill 修复;移除 `portable-pty`
依赖。移植以语义对齐为主,但 Windows 的 MSVCRT 命令行引用规则是逐字复制
(`src/pty/win.rs` 的 `append_quoted`),其原版权声明与许可全文见
[`THIRD_PARTY_NOTICES.md`](../../THIRD_PARTY_NOTICES.md) §1.1。

- **API 坍缩**:`pty::spawn(Command, PtySize) -> (Pty, reader, writer)`。
  master/slave 对象分离、运行时后端选择(`PtySystem` trait)、`ChildKiller`
  克隆、serial、Future 实现全部砍掉——zhimaterm 没有这些契约。
- **Windows**:windows-sys 0.61.2(lockfile 已有版本)直接链 kernel32 的
  `CreatePseudoConsole`/`ResizePseudoConsole`/`ClosePseudoConsole`,不再经
  shared_library 动态加载 sideload conpty.dll(目标即 Windows 10 1809+)。
  原local.rs 里为绕 ConPTY EOF 挂起手写的 `WaitForSingleObject` 等待线程
  内建为 `Pty::exit_signal()`。
- **Unix**:libc `openpty` + `pre_exec`(信号复位、`setsid`、`TIOCSCTTY`、
  `close_random_fds`)+ `TIOCSWINSZ` resize + EIO 即 EOF 的读包装 + writer
  Drop 补换行与 EOT;子进程直接用 `std::process::Child`。
- **Command**:argv + 少量环境覆盖,其余继承本进程;Unix 默认 Shell
  ($SHELL 可执行时 → passwd → /bin/sh)以登录 Shell 启动、落在 home、
  补 SHELL 变量,与 0.9.0 一致。

## 语义保持(对齐 0.9.0 实测源码)

- ConPTY flag 组合 `INHERIT_CURSOR|RESIZE_QUIRK|WIN32_INPUT_MODE`;stdio 三
  句柄显式置 `INVALID_HANDLE_VALUE`;未指定 cwd 时落在 USERPROFILE;
  argv[0] 经 PATH+PATHEXT 解析;命令行按 MSVCRT 规则引用;环境块 UTF-16、
  大小写不敏感覆盖。
- ConPTY 侧管道端点在 `CreatePseudoConsole` 返回后立即关闭。

## 与 0.9.0 的已知差异

- 不再从 `HKCU/HKLM\Environment` 注册表合并环境:桌面进程继承的用户环境
  已由系统在登录时合并(wezterm 的合并服务其 daemon 场景)。影响:应用
  运行期间系统级环境变量变更不会自动反映到新开的会话。
- Unix 显式程序路径(zhimaterm 当前不用)不再强制 home cwd 与 SHELL 注入。
- `try_wait` 在 `GetExitCodeProcess` 失败时返回错误而非 `Ok(None)`。

## 后果

- 构建树少 14 个传递依赖;`windows-sys` 成为 Windows 直接依赖。
- **ConPTY 启动握手**:INHERIT_CURSOR 下 ConPTY 会话建立即发 `ESC[6n`
  (DSR-CPR)并同步阻塞等宿主回复,应答由应用 VT 栈负责(既有行为不变);
  无应答的宿主(如测试探针)必须自行回复 `ESC[行;列R`,否则子进程控制台
  初始化永远挂起。
- 测试 seam:MSVCRT 引用规则、环境大小写覆盖、argv 拼接为纯单元测试;
  ConPTY 端到端冒烟(`spawns_shell_and_waits_for_clean_exit`,含 CPR 应答
  线程)覆盖管道、属性列表、`CreateProcessW` 与 wait 全链路;会话级行为由
  既有 `session_e2e` 套件验证。
- macOS/Linux 专项验证(requirements 既有要求)在首次跨平台构建时执行。

## Windows E2E 的平台边界(2026-10 实测)

session_e2e 的查询类用例原先只有 bash 探针,在 Windows 默认会话(PowerShell)
上整行解析即失败。补齐 Windows 原生测试时的实测结论:

- **cooked 输入收不到 VT 序列**:conhost 把写进输入管道的 ESC 序列解析/吞掉
  (ReadKey 一无所获),PowerShell 也没有 `stty raw` 等价物。解法是
  `zhimaterm-pty-probe` 二进制:子进程把自己的 stdin 切到
  `ENABLE_VIRTUAL_TERMINAL_INPUT`(VT 字节原样直传),裸读应答并比对。
- **conhost 会从子进程输出流拦截部分序列**:DSR(conhost 自行代答)、
  DECSET(模式开关,含 DECCKM 与鼠标上报)、OSC 10、CSI 14t。这些序列
  **永远到不了宿主终端的 VT 模型**——Windows Terminal 同样如此,是 ConPTY
  架构的既定行为,不是本项目的缺陷。
- 因此查询类用例分三类落地:
  - DSR、IME、滚动/搜索:Windows 原生跑(探针二进制 / PowerShell 命令),
    其余平台继续 bash 探针;
  - OSC 10、CSI 14t、DECCKM、鼠标上报:`#[cfg_attr(windows, ignore)]` 注明
    实测依据,应用侧行为由模型层单测(`colors::query_tests`、
    `device_queries`、`application_cursor_mode` 等)覆盖——这类行为在
    Windows 上不存在端到端的可观测路径。

## 移植过程中的关键坑

- `UpdateProcThreadAttribute` 的伪控制台属性值是 **HPCON 句柄值本身**,
  不是指向它的指针(MS EchoCon 示例与 alacritty 均直传)。传 `&con` 时子
  进程拿到的是垃圾控制台句柄,以 `0xC0000142`(STATUS_DLL_INIT_FAILED)
  死在控制台初始化——错误远在 PTY 读写之前,症状极具误导性。
- `PSEUDOCONSOLE_INHERIT_CURSOR` 的代价(见上文「ConPTY 启动握手」):
  开发期用它起裸 ConPTY 探针会表现为子进程挂起,必须先应答 `ESC[6n`。

## 附记(2026-10-18):exit_signal 穿层是设计,不做 reader 吸收

架构评审曾建议「ConPTY 子进程退出 ⇒ 读端出 EOF」吸收进 pty reader,让
`exit_signal`(win.rs 等待线程 → local.rs 挂载 → transport 槽位 → worker 的
`Wakeup::ChildExit`)四个触点在 seam 之下消失。**评估后不做**,理由是 ConPTY
的语义陷阱:**「子进程已退出 ≠ 管道已无数据」**——退出事件先到而管道里还压着
最后一屏输出时,「退出即合成 EOF」会截断尾部输出;「等管道排空再报 EOF」又回到
「EOF 可能永不到达」的原点,这正是本机制存在的原因,也是 Windows Terminal 与
portable-pty 都保留 exit-signal 形态的原因。四个触点各自很小(一个 Option 槽 +
一对 set/take + worker 一个唤醒分支),穿层反映的是这个概念确实跨层,不是实现
散落;`SessionExtras`(ADR-004 修订)也据此把它排除在事件载体之外。未来评审
请勿再以「层数」为由重提吸收,除非 ConPTY 本身提供了「排空后可靠 EOF」的
平台保证。

另:裸 ConPTY 测试里,持有 PTY 读写句柄的线程**句柄随线程结束而关闭会被
conhost 当成客户端断开,向初始化途中的子进程投递控制台事件**(0xC000013A
早夭,冒烟用例 2026-10-18 的失败根因)。测试线程应答 DSR 后需 park 保活,
与 worker 全程持有句柄的路径对齐。
