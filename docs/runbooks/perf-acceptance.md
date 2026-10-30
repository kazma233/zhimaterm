# 阶段 4 性能与资源验收

对应 `../context/requirements.md` §8（初始性能验收提案）与 §10（验收证据-性能测试）。测试代码在
`src/tests/perf_acceptance.rs`（重量级测试集中目录，与各模块内联的单测分开），全部
`#[ignore]` 门控，普通 `cargo test` 不受影响。

结论速览（2026-10-15 SSH 日常能力——认证叠加/端口转发/agent 转发/keepalive——落地后复跑，无回归；同日 ADR-017 会话输出日志改动 worker 排空循环后再复跑 6/6，P95 26.8ms、吞吐 355.3 MiB/s、其余项同量级；验收环境见下）：

| §8 验收项 | 阈值 | 实测 | 结论 |
|---|---|---|---|
| 8 个会话同时运行 | 全部存活可用 | 4 SSH + 4 本地并发 spawn 357µs，全部跑完标记命令并确认存活 | 通过 |
| 200x80 网格 + 10,000 行默认历史 | history ≥ 10,000，快照行列正确 | history = 10,000；灌 12,000 行 8.5ms，snapshot 594µs | 通过 |
| 10 MiB/s 刷屏时输入 P95 | P95 ≤ 50ms | P50 20.8ms / **P95 30.9ms** / P99 31.5ms | 通过 |
| 1 GiB 传输期间其他会话可交互 | 按键回显 < 50ms | 20 次按键回显最大 9.0ms、平均 8.0ms；B 会话能完整执行命令 | 通过 |
| 可读写时取消 2 秒内到终态 | ≤ 2s | **3.3ms** 到 `Cancelled` | 通过 |
| 连接无响应 2 秒后强制关闭 | 任务失败 + 会话关闭 ≈2s（容忍 2~5s） | 任务失败终态 **2.0002s**、会话关闭 **2.0008s** | 通过（见 4.1） |
| 100 会话创建销毁无残留 | 子进程 / fd 残留 ≤ ±5 | 100 个 SSH 会话 14.7s；子进程 0 → 0，fd 6 → 5 | 通过 |
| 附：SFTP 稳态吞吐（§8 未要求） | — | **355.6 MiB/s**（整体均值 343.5；修复前 0.8 MiB/s，见 4.2） | 参考 |

## 1. 验收环境

| 项 | 值 |
|---|---|
| 机型 / 芯片 | Mac mini（Apple M4，10 核） |
| 内存 | 16 GB |
| 系统 | macOS 27.0.1（Darwin 27.0.0 arm64） |
| 工具链 | rustc / cargo 1.99.1（与 rust-toolchain.toml 一致） |
| 构建档 | `--release`（见下文「为什么必须 release」） |
| SSH 服务端 | 回环 2222 端口一次性 sshd，公钥认证，见 `ssh-e2e.md` 第 1 节 |

## 2. 复现命令

```bash
# 1) 起 sshd（通常已在跑）：照 docs/runbooks/ssh-e2e.md 第 1 节执行
# 2) 跑全部性能验收（约 80 秒；须 --test-threads=1，用例间共享环境变量与 sshd）
D=/tmp/zhimaterm-sshd
ZHIMATERM_PERF=1 \
ZHIMATERM_SSH_HOST=127.0.0.1 ZHIMATERM_SSH_PORT=2222 \
ZHIMATERM_SSH_USER="$(whoami)" ZHIMATERM_SSH_KEY=$D/client_ed25519 \
  cargo test --bin zhimaterm perf_ --release -- --ignored --nocapture --test-threads=1
# 单跑某项：把 perf_ 换成用例名，如 perf_input_p95_under_flood
```

环境变量门控：本地类用例需要 `ZHIMATERM_PERF=1`；SSH 类用例需要 `ZHIMATERM_SSH_*`（没有时打印跳过原因）。
所有实测数字经 `eprintln!` 输出，`--nocapture` 可见。

Windows 原生跑法（cmd，本地项直接可跑，SSH 项的 sshd 准备同上）：

```cmd
set ZHIMATERM_PERF=1
cargo test --bin zhimaterm perf_ -- --ignored --nocapture --test-threads=1
```

平台口径差异：本地会话默认 shell 是 PowerShell，标记命令用字面量 `echo`（不依赖
PATH 里碰巧存在的 Git printf.exe）；刷屏负载是 PowerShell 版（`[Console]::Write`
原位重绘 + `ReadKey($true)` 无回显读键做应用层回显，负载形状与 POSIX 版一致）；
残留对照的「子进程数」用 Toolhelp32 快照按父进程过滤、「fd 数」用进程内核句柄
总数替代（泄漏断言语义不变）。Windows 下 debug 构建也能跑通全套（VT 解析吞吐
足够，见 3.3 的 Windows 实测），数字口径仍以 release 为准。

**为什么必须 release**：debug 构建下 VT 解析吞吐不足 10 MiB/s，回显字节被压在输出积压
后面永远出不了帧——那是构建配置问题，不是被测语义。功能回归（`cargo test --bin
zhimaterm`，不带 `--ignored`）仍在 debug 下全绿。

**已知偏离（测速方法限制，非产品行为）**：`perf_input_p95_under_flood` 的刷屏是「原位
重绘」而不是滚动输出（字节速率、VT 解析负载与滚动刷屏相同）。原因见该用例文档注释：
10 MiB/s 的滚动输出每 50ms 推进约 2600 行，回显字符在网格里只存活约 1ms，低于 8ms 帧
间隔，任何画面帧都捕捉不到回显，逐键延迟无从测量；且内核回显会被刷屏光标处的后续字节
立刻覆盖，因此回显由刷屏循环自己完成（`stty raw -echo` + 块间非阻塞读 + 定位写）。
被测路径（按键 → 核心命令队列 → PTY 回写 → 解析 → 帧）与真实刷屏完全一致。

## 3. 各项方法与实测数字

实测原始输出摘录（2026-10-10 唤醒管道落地后整套连跑，`--test-threads=1`，总时长约 33 秒）：

### 3.1 8 会话并发（`perf_eight_sessions_run_concurrently`）

方法：同时 spawn 4 个 SSH + 4 个本地会话（无 `ZHIMATERM_SSH_*` 时自动退化为 8 本地并
注明），逐个解析主机密钥确认，各自跑一条 printf 标记命令并等到输出，确认 8 个全部存
活后逐个 close 并等退出事件。

```
8 个会话并发 spawn（4 SSH + 4 本地）：128.25µs
8 个会话全部跑完标记命令并确认存活：132.680083ms
```

### 3.2 大网格与历史（`perf_large_grid_and_history`）

方法：`TerminalModel::new(200, 80)`，灌 12,000 行（超过默认历史上限），断言快照列数
200、行数 80、history ≥ 10,000，且最新行可见、最早行已进入历史。

```
灌入 12000 行耗时 5.559542ms，snapshot 耗时 403.959µs，history = 10000
```

### 3.3 刷屏下的输入延迟（`perf_input_p95_under_flood`）

方法：单会话按构造输出约 10 MiB/s（54 重绘块 × 400 拍，实测 13.3 MiB/s，≥10 即满足
「持续输出 10 MiB/s」的前提），期间发送 200 次按键，逐次计时「`key()` 返回 → 回显字
符出现在画面帧」，统计分位数。

```
刷屏期间 200 次按键回显延迟：P50 22.701416ms / P95 33.456833ms / P99 35.634584ms（均值 19.168924ms）
刷屏实际速率：13.4 MiB/s（209736000 字节 / 14.89s，按构造目标 10.0 MiB/s）
```

Windows 实测（Windows 11 / debug 构建 / PowerShell 版负载，2026-10-18）：P50
15.9ms / P95 48.3ms / P99 63.3ms（均值 25.1ms），全程 ~32s，通过（P95 贴近但
未超 50ms 上限；debug 构建的数字，仅作可跑性证据）。注意 settled 判据必须是
「重绘区全数字」这类肯定式谓词：早期版本用「无字母」，空白首帧会立即命中，
Windows 上曾把「负载没跑起来」量成空闲延迟假通过。

「UI 事件队列不无界增长」由结构保证（命令 256 / 帧 1 / 控制 8 全部有界通道），未单独
计量。

### 3.4 1 GiB 传输期间的交互性（`perf_gib_transfer_keeps_sessions_interactive`）

方法：SSH 会话 A 上传 1 GiB 稀疏文件（`/tmp` 下，跑完删）；等传输进入稳态（进度
≥ 32 MiB）后，在本地会话 B（已清屏、停在 `read` 上）连续发送 20 个字母并逐次测回显
延迟，再完整执行一条命令；随后 `cancel_task` 测取消到终态的耗时。

```
上传已启动（首个进度 30000 字节，稳态到达 32 MiB，用时 96.949083ms）
1 GiB 上传期间会话 B 的 20 次按键回显延迟：最大 8.999542ms，平均 7.962433ms
取消到终态耗时 3.27575ms，结果 Cancelled("上传已取消")；取消前已传 109051904 / 1073741824 字节
上传稳态吞吐（交互测量窗口）：355.6 MiB/s，整体平均 343.5 MiB/s
```

### 3.5 僵死连接强制关闭（`perf_stalled_connection_force_closes_within_2s`）

方法：测试内起一个进程内 TCP 黑洞代理（监听回环随机端口，双向转发到真实 sshd；转发
停止后不再读取也不关闭，让客户端侧发送缓冲被内核塞满，连接保持 ESTABLISHED）。会话
经代理连接并起 512 MiB 上传，数据开始流动后切断转发，测「切断 → 任务失败终态」与
「切断 → 会话关闭」两个耗时。

```
僵死判定到任务失败终态：2.000199834s，结果 Failed("连接无响应，任务被强制终止")
会话强制关闭（SFTP 传输停滞，连接已强制关闭）：2.00077425s
```

### 3.6 100 会话无残留（`perf_hundred_sessions_leave_nothing_behind`）

方法：循环 100 次「spawn → 跑标记命令 → close → 等退出事件」（SSH 优先，无环境变量
时退化为本地）。前后对照子进程数（Unix：`pgrep -P <self>`；Windows：Toolhelp32
快照按父进程过滤）与文件描述符数（Linux `/proc/self/fd`，macOS `/dev/fd`；Windows
以进程内核句柄总数替代），容差 ±5。基线采样先等前序用例的拆卸退场稳定（连续两次
计数不变），串行连跑时前一个用例的 shell/ConPTY 宿主退出途中会被误算成泄漏。

```
100 个会话循环耗时 14.995694917s；子进程 0 → 0，fd 6 → 5
```

## 4. 发现的问题与处置

### 4.1 连接无响应时「2 秒强制关闭」不生效 —— 已修复

- **原现象**：上传进行中把连接变成黑洞（TCP 保持、数据不被消费），任务永不失败、会话
  永不关闭；取消命令也无法处理。
- **根因（两层，均已实测确认）**：
  1. `sftp_task.rs` 的 `SshRemoteFs::write/read` 遇 `WouldBlock` 在**函数内部**以 1ms
     `poll_socket` 无限重试，从不向任务层返回 `Blocked` → `TransferTask::blocked_since`
     永远不置位，`ssh.rs` 的 `STALL_TIMEOUT` 检测等不到输入。
  2. 即使置位：pump 返回前要在 `pump_loop` 作用域里 drop `ssh2::Channel`/`Sftp`/`File`，
     而 **ssh2-rs 在这些句柄的 `Drop` 里会把会话临时切回阻塞模式**再调 libssh2
     （`sess.set_blocking(true)`，见 ssh2-rs 0.9.6 `sftp.rs` 的 `File`/`SftpInnerDropWrapper`
     Drop 实现）；阻塞模式下 libssh2 遇 EAGAIN 会等 socket 就绪——无响应连接上实测
     drop 一个打开的 SFTP 文件句柄要 **~3 秒**，彻底僵死时无上限。pump 卡在 drop 里，
     会话级终态（`state.exit`）根本设置不到，worker 永远收不到 `Exited`。
- **修复**：
  - `SshRemoteFs::write/read` 首次 `WouldBlock` 即返回 `StreamProgress::Blocked`，轮询
    交给 pump 的节拍（不再函数内自旋）。
  - pump 的所有退出路径统一走 `finish_session()`：`disconnect` 之后**从 fd 层
    `libc::shutdown(socket_fd, SHUT_RDWR)`**。socket 一旦 shutdown，读立刻 EOF、写立刻
    报错（不再是 EAGAIN），libssh2 内部所有等待/重试立即解开，drop 全部毫秒级完成。
- **修复后实测**：任务失败终态 2.103s、会话关闭 2.103s（两者相隔 ~0.2ms），用例 2.2 秒
  通过。回归：单测 94 全绿、SSH e2e 10 项全绿、perf 套件 6/6 通过。

### 4.2 SFTP 传输吞吐仅约 1 MiB/s —— 已修复（0.8 → 356 MiB/s）

- **原现象**：回环 sshd、release 构建，上传稳态吞吐约 0.9~1.0 MiB/s（32 MiB 用时
  36.5~37.5s）。1 GiB 文件按此速率需约 18 分钟。
- **测量（诊断统计）**：每次 `libssh2_sftp_write` 稳定写 ~29KB（`avg_done=29131`），
  1000 次调用累计耗时仅 128ms、阻塞仅 32 次，但墙钟 32.5s——**99.6% 的时间不在写调用
  里，而在轮次之间**：每轮约 32ms。
- **根因**：pump 每轮末尾的等待量由 shell 通道状态决定（`pending_read`）；传输期间
  shell 空闲，`channel.read()` 每轮都返回 `WouldBlock` → 命中 50ms 轮拍——这个等待是
  为纯 shell 场景防忙等调的，现在每次 ~29KB 的 SFTP 写也被拖成 ~32ms/轮。
- **修复（终态：唤醒管道）**：先按「有任务推进就不睡」治标；随后把等待机制收敛为
  真事件驱动——pump 建一对唤醒管道，所有入队方（worker 的控制/写入、UI 的 SFTP
  命令）入队后 poke 一字节，`poll` 同时等 socket 与管道。等待预算只剩三种情形：
  任务等 socket → 僵死剩余额度；任务在推进 → 不睡；空闲 → 无限等待（真零轮询、
  零空闲唤醒；命令/resize/关闭/SFTP 操作到达即醒）。规则与约束见 ADR-005。
- **修复后实测**：稳态 **355.6 MiB/s**、整体均值 343.5 MiB/s（提升约 400 倍）；32 MiB
  稳态从 38.5s 降到 0.13s；取消 3.3ms、传输期间回显 9.0ms 不退化；perf 套件总时长
  从 70s 缩到 33s。回归：单测 94、SSH e2e 11 项、perf 6 项全绿。

### 4.3 噪声记录

- 非 root sshd 的日志里有 `Attempt to write login records by non-root user`，与
  `ssh-e2e.md` 记录一致，不影响结果。

## 5. 人工交互验收清单（无法自动化，待人工执行）

以下行为依赖真实 GUI、输入法与用户确认流程，验收时按下表逐项人工操作并勾选。
环境：`cargo run --release` 打开 ZhimaTerm，SSH 会话用 2222 服务端
（`127.0.0.1` / 当前用户 / 私钥 `/tmp/zhimaterm-sshd/client_ed25519`）。

| # | 项目 | 操作步骤 | 预期 | 状态 |
|---|---|---|---|---|
| 1 | CJK 宽字符 | 本地会话执行 `echo 中文宽度测试；printf '中文ab\n'`；SSH 会话重复 | 宽字符占两格且不与半角错位、不截半；光标位置正确；滚动回看不错行 | 已验收（2026-10-22 用户实机，通过） |
| 2 | 组合字符与 Emoji | 执行 `printf 'e\u0301 \xf0\x9f\x98\x80\n'`（é 与 😀） | 组合字符叠在前一字格上；Emoji 占两格不花屏 | 已验收（2026-10-22 用户实机，通过） |
| 3 | 输入法（IME） | 切到中文输入法，在提示符输入拼音、选字、上屏；再用英文输入法混打 | 组屏期间不上屏内容；上屏后文本按原样进入行缓冲；退格按字删除 | 已验收（2026-10-22 用户实机，通过） |
| 4 | 鼠标协议 | `ssh` 到任意主机跑 `vim`/`htop`（开启鼠标上报）滚轮、点击、拖动；退出后回到 shell 再拖选 | 程序内滚轮/点击上报给程序（vim 滚屏、htop 选中）；退出后拖选变成本地选区，指针形状随程序切换（箭头/文本） | 已验收（2026-10-22 用户实机，通过） |
| 5 | 复制粘贴 | 拖选含中文与颜色的一段输出 → Cmd-C → 粘贴到本会话与另一会话/其他应用；程序开启 bracketed paste 时（`vim` 内）粘贴多行 | 复制内容与屏幕一致（含宽字符边界）；粘贴进 vim 不触发自动缩进错乱（bracketed paste 生效）；粘贴内容不含 ESC 序列 | 已验收（2026-10-22 用户实机，通过） |
| 6 | 覆盖上传确认弹窗 | SFTP 面板把文件拖/上传到远端已有同名文件的位置 | 弹确认框，显示目标名；取消则不动目标；确认后走覆盖提交，内容为新材料 | 已验收（2026-10-22 用户实机，通过） |
| 7 | 递归删除确认弹窗 | SFTP 面板对含子目录的远端目录发起删除 | 弹确认框（说明条目数）；确认后逐层删除完成；取消则目录原样保留 | 已验收（2026-10-22 用户实机，通过） |
| 8 | 多标签与状态隔离 | 开 1 本地 + 1 SSH 标签，各自跑 `top` 刷屏，来回切换、关掉其中一个 | 各自画面/滚动位置独立；关闭一个不影响另一个；关闭的标签无残留输出 | 已验收（2026-10-22 用户实机，通过） |
| 9 | SSH 会话关闭后的句柄失效 | shell 里 `exit`，SFTP 面板仍可浏览；再关闭会话（标签） | shell 退出后面板仍可用（ADR-005）；关闭会话后面板立即报连接已关闭，进行中传输进入失败终态 | 已验收（2026-10-22 用户实机，通过） |

## 6. 与 §8/§10 证据要求的对照

- §10「性能测试：按第 8 节固定输入数据和目标环境执行，并记录结果」：本文档第 3 节。
- §10「人工交互验收」：第 5 节清单，验收时逐项勾选。
- §8 七项全部通过；两处修复（4.1 僵死关闭、4.2 传输吞吐）均已落地并回归。
