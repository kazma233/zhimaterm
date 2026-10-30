# ADR-028:本地进程探针与右侧边栏(仅默认 Shell)

状态:已接受(2026-10)

## 背景

远端会话有监控底栏(CPU/内存/磁盘/网速),本地格按 ADR-016 连底栏都不画。但本地
开发者同样有「这个 shell 里跑了什么、占了多少内存、哪些端口被谁监听」的观察需求
——开发服务器卡了想找内存大户、端口冲突想找占用者,现在的出路是去终端里敲
`ps`/`ss`,把观察工具挡在被观察对象里。

需求与口径(与用户逐项确认):

- 进程 = 以 shell 根 PID 为祖先的全部存活后代(树形,含后台 job);Linux 僵尸跳过;
  daemon 化脱树的进程天然不在内。
- 端口 = 仅 TCP LISTEN,按「端口 + 进程」合并行(v4/v6、SO_REUSEPORT 的多绑定收进
  一行的次要文字)。
- ~~只读:不提供 kill 等任何操作~~(**2026-10-28 修订**:应用户要求加回操作能力——
  端口行可点开端口详情弹层,进程与端口详情均提供「结束进程」(确认后执行);根
  shell 不给结束入口。原文保留:无 CPU%、无 UDP、无快捷键、无状态持久化)。
- WSL 不做(见下)。

## 决策

### 采集:三平台三实现,树与合并只有一份

新增 `probe` 模块,平台层(`probe/{linux,macos,windows}.rs`,`target_os` 级门控——
macOS 与 Linux 同为 unix 但 API 完全不同,不能沿用 pty 的二分)只负责采集「全量
进程表 + LISTEN socket 表(关联 PID)」,树构建(`build_tree`)与端口合并
(`merge_ports`)是公共纯函数,口径单点维护:

- **Linux**:`/proc` 全解析(stat 的 ppid 与 comm 按**最后一个** `)` 切、status 的
  VmRSS、cmdline;端口是 `/proc/net/tcp{,6}` 的 LISTEN inode ↔ 树内进程 fd 的
  `socket:[inode]` 两步匹配。fd 只读树内后代——应用是它们的祖先,Yama
  ptrace_scope=1 下也放行。
- **macOS**:进程与内存走 libproc(系统库 `extern "C"` 直连,无新依赖;
  `PROC_PIDTBSDINFO` 拿 ppid、`PROC_PIDTASKINFO` 的 resident_size 与 VmRSS 同
  口径),完整 argv 走 sysctl `KERN_PROCARGS2`(`ps` 同款系统接口,同一无新依赖
  口径——proc_pidpath 只有可执行文件,脚本与参数都在 argv 里);端口归属无
  公开 API,按事实标准调系统自带的 `lsof -nP -F pcn -iTCP
  -sTCP:LISTEN`(`-nP` 关掉主机名/服务名反解,数字输出才可解析)。lsof 失败只
  降级端口区(`ports_unavailable`),进程区照常。
- **Windows**:ToolHelp 快照(依赖树已有)+ `GetProcessMemoryInfo`(WorkingSet,
  feature `Win32_System_ProcessStatus`)+ `GetExtendedTcpTable` 的 OWNER_PID
  LISTENER 表(feature `Win32_NetworkManagement_IpHelper`;注意参数序
  (buffer, size, Order, Family, Class, Reserved),Order 与 Family 传反不会报错
  只会查空)。PID 重用由树构建的 visited 集合防环。

内存列三平台口径统一为「驻留内存」。命令行尽力而为:Linux 有 cmdline、macOS 有
完整 argv(sysctl KERN_PROCARGS2,取不到回退可执行路径)、Windows 于 2026-10-22
补齐:完整命令行读目标 PEB 的 CommandLine(`src/peb.rs`,与 ADR-039 的 cwd 采集
共用),经 `CommandLineToArgvW` 按系统规则切 argv;可执行路径走
`QueryFullProcessImageName`,不再恒 None。展示名三平台统一走公共
`display_name`:argv[0] 属常见解释器且 argv[1] 非选项时显示脚本名——跑 `pi` 的
node 进程显示 pi 而非 node(Windows 的 `.exe` 后缀与反斜杠路径同样识别)。

### 结束进程:发信号即刻返回,退场由采样闭环确认

2026-10-28 加回操作能力(修订上文「只读」口径)。kill 放在 `ProbeHandle` 上由
UI 线程直接调——发信号是微秒级系统调用,不值得经过采样线程走请求-响应通道;
发出后不等待,目标是否真退场由下一拍采样回答(≤2 秒):进程退场/端口行消失
即弹层被退场检测顺手关闭,列表里行消失。平台语义差异显式接受:Unix 发
SIGTERM(可优雅退出,被忽略时用户可重发),Windows `TerminateProcess` 硬杀
(系统无 TERM 等价物)。

两道安全边界:

- **根 shell 拒绝**:`ProbeHandle::kill` 拦截根 PID(杀根 = 杀会话本体),UI 侧
  根进程的详情弹层也不渲染结束按钮(双保险,入口与能力各管一道)。
- **PID 复用复核**:从采样到用户点击之间目标可能已死、PID 被复用成别的进程,
  盲杀会误伤。`kill_process(pid, expect_exe)` 在发信号前一刻取平台现行可执行
  路径(macos `proc_pidpath`、linux `/proc/[pid]/exe`、windows
  `QueryFullProcessImageNameW`,与采样期同一 API 保证口径自洽)与采样值比对,
  不一致即拒杀;采样期路径本就缺失(None)时无从比对,退化为 kill 自身的
  存在性检查,残余误杀窗口标注为已知边界。

UI 侧对齐项目破坏性动作口径(ADR-002/SFTP):「结束进程」先经**面板内二次确认
弹层**(SFTP 确认弹窗同款:面板级遮罩 + danger 按钮,叠在详情弹层之上,取消/
点遮罩回详情)。确认弹层不走全局浮层栈(ADR-044)——那是窗口级居中大卡片的
空间,与面板级详情卡视觉脱节(实机否决,Dialog confirm 形态的全局通道继续无
消费方);也不在详情卡内就地切换确认态——丢掉「正在确认什么」的上下文。
发送结果就地显示(成功等待中/失败原因),失败(ESRCH/EPERM/OpenProcess 拒绝)
不静默。

### 生命周期:开即采、关即停

数据流沿用 MonitorReady 模式:`local::open` 创建 `ProbeHandle`(根 PID 与
exit_signal 同样在 pty 移入 `LocalControl` 前取出)→ `SessionExtras.probe` →
worker 发一次 `ControlEvent::ProbeReady` → 面板实体持有句柄。

与远端监控的关键差异:远端通道随连接常驻,本地采样线程**随面板开合启停**——
`ProbeHandle::start()` 起线程并返回本运行段的事件流,`stop()` 后线程在一个睡眠
分片(100ms)内退出。空闲本地格零唤醒的 worker 约束(README 性能一节)不被破坏。
代价是每次展开面板有 ≤2 秒「采样中…」空窗。连续 3 次采集失败才判终态
(lsof 允许偶发失败),终态与远端监控同观感:显示原因,终端会话不受影响。

### 布局:常驻底栏入口 + 停靠面板

- **入口**:本地默认 Shell 格渲染一条底栏,与远端状态底栏同位置同款(30px 高、
  上边框、右端开关),开关是一枚「进程」chip(样式对齐底栏 SFTP 开关)。信息区
  留空——本地格无状态文案,启动与故障信息仍在网格中央(ADR-016)。底栏在本地格
  整个生命周期恒在;`probe_capable` 由会话目标在视图构造时判定(布局从首帧起
  恒定),面板实体未就绪(ProbeReady 未到的毫秒级窗口)渲染不可点的占位开关。
  此前是右缘 32px 常驻按钮列,实机看过后按用户口径改到底栏。
- **面板**:停靠式 flex 子元素:展开时贴终端区右缘,终端让宽,PTY 按新列数重排;
  收起即不渲染。默认 400px、左缘拖宽范围 240–960、默认收起、不持久化。
  曾实现过「宽度变化由窗口吸收」(`Window::resize` 同步加宽窗口、终端列数不变),
  实机查看后用户否决——窗口突然变宽的交互很突兀,回退为终端让宽。
- **面板内部**:顶部汇总行(N 个进程 · 合计内存),其下是单一滚动区:进程树在前、
  端口段紧随其后(「监听端口 · N」小节标题分界——端口行可点后端口段有了交互
  身份,纯细线不足以表达「另一类内容」),合并内容超出面板高度时整体滚动——
  端口不钉底、中段不留空;无监听端口时端口段整个不渲染,端口区不可用只显示
  原因。进程行按
  真实谱系深度累进缩进:树根是会话 shell 本身,后代沿真实父子链逐层缩进(谁拉起
  了谁一眼可见;用户先后否决过独立 spacer 与「后代统一一格」,最终回到按深度),
  并叠加树导线(│/├/└,1px 实色 div 画线——绝对定位盒子不给尺寸就是零尺寸,
  border 画不出;竖线统一在格中点跨行对齐;元数据从 walk 平铺
  序单遍推导,末子判定取「向后第一条深度不大于自己」——紧邻下一条可能是长子)。
  行宽由 overflow_hidden 收敛;与进程名重复的命令行不渲染副行(Linux 无参数时
  argv0 与进程名重复,观感是孤儿行),macOS 命令行是完整 argv,无参数也照常渲染
  第二行。行可点击开进程详情弹层(面板级遮罩,SFTP 对话框同口径,只盖面板区域;
  Card 宽度按面板实宽出,不吃字号缩放):完整命令行与可执行路径按词换行、不设
  省略,字段值点击即复制整段(文字短暂变绿作反馈,缺失值不接点击;值元素 id
  必须按字段区分——gpui 点击状态挂在 id 上,同 id 共用会丢点击);详情挂
  pid,目标进程退场由下一拍采样顺手关闭。端口行整行可点开端口详情(交互与
  进程行同款):端口、协议、绑定地址、占用进程与该进程的路径/命令行,同端口
  换进程接管视为过期(详情按 (端口, 进程) 二元组绑定而非端口本身);弹层内
  「结束进程」见上文 kill 决策。树内子节点
  按 pid 升序固定排序、滚动位置跨 2 秒刷新保持(行序稳定所以滚动锚不跳)。面板
  内容是外部进程数据、长度不可控,全部行 overflow_hidden 收敛,面板根再兜一层
  ——任何宽度失手都不能画出面板。

### WSL:V1 不提供

WSL 会话的真实进程树与端口在 Linux 子系统里,宿主 Windows API 只能看到
`wsl.exe`——这是能力边界不是实现细节。已论证过的可行路径是经
`wsl.exe -d <distro> --exec` 起管道子进程跑 /proc 采集(与远端监控同构,锚点用
WSLENV 注入会话标记定位 shell),实现成本约等于再加半个平台。挂起待后续单独
决策;届时 WSL 格按钮列的出现方式需重审「布局恒定」结论。

## 后果

- `Pty::pid()` 暴露根 PID(Unix 转发 `child.id()`,Windows `GetProcessId`);
  采样期持有 PID 而非句柄——根 shell 存活到会话结束,PID 在此期间不会被夺,
  树内子孙的 PID 复用由 ppid 可达性自然过滤。
- ADR-016 的「本地格没有监控」前提被本决策修订(本地格有探针侧边栏),但底栏
  结论不变:本地格仍无底栏,启动/故障信息仍走网格中央,见 ADR-016 修订注记。
- 混合分屏中本地格探针底栏与远端格状态底栏同高同款;本地格展开探针面板的让宽
  属用户显式开合,接受。
- Linux 侧有真实 /proc 端到端测试(树、两级子进程、LISTEN 端口归属);macOS 与
  Windows 的采集代码已在 Linux 侧对 probe 模块交叉 cargo check 过(见
  platform-verification),运行验证按该流程在实机进行——**unverified** 项:
  lsof -F 在 macOS 的实际输出。libproc 结构体布局一项已于 2026-10-19 实机验证
  并爆雷:手写布局与 sys/proc_info.h 不符导致进程表恒空,已修复(改用 libc 导出的
  `proc_bsdinfo`/`proc_taskinfo`),记录见 platform-verification。同日新增的
  sysctl KERN_PROCARGS2 argv 采集与解释器脚本名恢复已实机 `cargo test` 验证
  (含自身 argv 读取与 shebang 脚本端到端两例),命令行口径与同类工具用 sysinfo
  `cmd()` 取到的结果一致。Windows 侧已于 2026-10-22 实机验证:`cargo test`
  全量(采样启停端到端)含 `own_listening_sockets_appear_in_the_sample`——本进程
  监听临时 v4/v6 端口后采样应归到自己名下,GetExtendedTcpTable 的参数序与
  字节序由此实证;argv/exe_path 的实机表现见 manual-acceptance 同日项。
- `/proc` 与 lsof 的文本解析全部是纯函数单测(fixture 基于真实输出);
  `probe_handle_delivers_sample_then_stops` 覆盖启停(三平台)。kill 的实机
  端到端 `kill_checks_identity_terminates_and_rejects_root`(三平台):身份
  复核不过拒杀且目标仍活、放行的 kill 让子进程退出、已退出再杀报「已退出」、
  杀根被拒绝——2026-10-28 于 macOS 实机通过;Linux/Windows 侧经临时 shadow
  crate 三目标交叉 `cargo check` 验证类型,实机运行为 unverified(待验收项
  见 manual-acceptance)。同日端口详情弹层与面板内二次确认弹层的
  UI 全流程经用户 macOS 实机人工验收通过(含确认卡叠详情卡之上、取消回详情、
  kill 后采样闭环收尾)。
