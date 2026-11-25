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
- 窗口顶栏与侧栏动作行（2026-11-16 小节，ADR-057 及修订）：macOS 全套
  （红绿灯留位、拖拽/双击、搜索过滤、图标入口、几何往返、全屏切换）；
  Windows/Linux 实机验证与冒烟步骤见小节。
- 终端搜索条位置（2026-11-16 小节，ADR-015 修订）：右上角浮动条的人工
  验收（不挤占网格、增量搜索与 Esc 语义不变）。
- 2026-11-17 批次（ADR-058/059，ADR-015/023/055 修订）：格右上角动作
  图标行（三个图标动作生效、满 8 格拆分钮禁用、tooltip 与 hover；点哪格
  拆哪格、搜索开在本格）；搜索条与转发卡片下移后不与动作行重叠；右键
  菜单只剩两个拆分项；设置模态「打开设置文件」按钮（动作区与冻结横幅
  两处）在配置的编辑器里打开 settings.json；打开设置模态重读盘上值
  （手改 settings.json 后预填可见、应用不覆盖外部改动；修好坏文件后
  冻结解除）；连字开关即时生效（Fira Code 看 `->`）、字体特性非法输入
  报错、合法输入（如 `zero 1`）应用后生效。
  已验证部分：图标行在 macOS 实机渲染正确（三个图标形状与位置）；
  「打开设置文件」按钮与连字开关的存在/点击由渲染级测试守住
  （`the_settings_modal_renders_the_new_controls_and_the_open_file_button`）。
  连字成形的实测结论（2026-11-17，ADR-059 已知限制段）：macOS CoreText
  整形对 calt 完全不应用（默认与显式 calt=1 都逐字符），连字开关在
  macOS 两态同显——上游/平台限制，非设置实现缺陷；连字测试方法：字形
  数判据（`->` 成形 = 1 字形，逐字符 = 2 字形），生产侧经网格绘制探针
  或独立 CTLine 实验取证（`cargo test` 环境 gpui 字体枚举为空，整形
  无法在单测里验证）。
  未验证：Windows（DirectWrite）与 Linux 上连字开关/字体特性是否有
  可见效果，待实机验证——方法同上（字形数判据）；tooltip 文案、满格
  禁用态的实机观感需真人过一遍。
  2026-11-25 更新：格右上角动作行与终端右键菜单已由 ADR-063 退役，
  本批次中动作行/右键菜单相关的验收项作废；搜索条与转发卡片回到右上角
  8px，右键改为转发前台程序。
- 分屏焦点描边与网格留白（2026-11-25，ADR-064）：多格切焦点时 2px 描边
  跟随且框住整格（含停靠面板与状态底栏）、单格不显示；网格四周 6px 留白
  在单格/分屏/SFTP 与状态底栏在场时不遮挡文字；切焦点不触发 PTY 重排；
  点击留白只聚焦；回看标记在左侧留白内、颜色随主题 accent（明度偏一档）、
  沿实际滚动方向 10 行渐隐拖尾，停止滚动 1s 内整体淡出消失。
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
- 窗口顶栏（ADR-057，2026-11-16）：macOS 全量收尾检查通过；Windows/Linux
  分支（caption 按钮、Client 装饰请求与按钮渲染）是同源 gpui API 的统一代码，
  rustc/clippy 在 macOS 侧已过，但两平台均未编译与实机验证——待 Windows/
  Linux 桌面各跑一遍 `cargo check --all-targets` + 冒烟（步骤见
  `manual-acceptance.md` 窗口顶栏小节）。Linux CSD 的阴影/圆角精修
  （`set_client_inset`/`set_client_corner_radii`）后置。
- 遗留（2026-11-16 发现，先于顶栏批次存在）：`wsl::tests::wsl2_size_reads_ext4_vhdx`
  与 `wsl1_size_sums_directory` 在 macOS 上失败（干净 main 同样失败，
  stash 对照确证；WSL 批次应只在 Windows 验证过）；`install_args` 的
  dead_code 警告已随本次补 `#[cfg(windows)]` 门。两个失败用例需要 WSL
  批次的作者在 Windows 上复核门控或修断言。
- CI 三平台 matrix：暂缓（见「明确不管/后置」）。
- ADR-060 步骤 2/3 剩余（2026-11-21）：ghostty 后端在 **Linux** 的真实使用
  验证（表格对齐、TUI、IME、SSH 长尾；Windows 已于同日实机过探针三内核
  对照，见 platform-verification）；验证通过后把默认后端从 alacritty 切到
  ghostty（切默认是单独决策，需先更新 ADR-060 再动行为）。

## 功能池 P2 剩余

Zmodem（rz/sz）、GBK/编码切换、X11 转发、chmod/属主编辑、传输限速/断点续传/并行、
会话分组、密钥生成器、快捷键自定义/多窗口；另有兼容性小项：小键盘
APP_KEYPAD 编码（GPUI 不区分小键盘按键，缺能力；自 alacritty-lessons 调研
归档时移入）。（手动重命名已于 2026-10-27 落地，ADR-047。）
（背景与排序见 `mobaxterm-gap-and-priorities.md` §3 P2。）

## 技术债

- **字体覆盖与 fallback**（2026-11-21 探针实测发现，决策单独处理）：主字体
  （JetBrains Mono）缺字形时无 fallback 链——块元素 `▔`(U+2594) `▁`(U+2581)
  `▏`(U+258F) 与双线制表符 `═║╔╗╚╝` 显示异常，键帽 `1️⃣`（VS16 序列）无
  emoji 字形回落、`⚠️` 呈文本样式。方向：调研 kitty/wezterm/ghostty 的
  字体 fallback 做法，给终端网格加可配置的 fallback 字体链（emoji 至少
  落 Segoe UI Emoji/平台等价物）；块元素/制表线先核对 ADR-034 几何表是否
  全覆盖（`▔▁▏` 理论上在 U+2580–259F 内，实测行为待查证是几何缺口还是
  字形路径）。关联 unverified：彩色 emoji 三平台字体回退表现。
- （2026-10-22 的两条——known_hosts 并发写丢失更新、settings 测试 fixture 的
  Unix 绝对路径——已修，见 ADR-002 落盘串行化段与 `settings::test_paths`。）

## unverified

- ~~连字（ligature）跨格成形未实测~~（2026-11-17 实测关闭：macOS CoreText
  对 calt 完全不应用，连字从不成形，见 ADR-059 已知限制段；Windows/Linux
  待实机用同一字形数判据验证）。
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
- **明确不做（2026-11-17 讨论）**：滚动历史上限按字节/内存设限。维持行数口径
  （ADR-035）：后端语义无法统一——alacritty 上游只有行数 API，ghostty 原生
  `SCROLLBACK_MAX_BYTES` 被有意留空（行数才是口径；讨论时的第三后端 termwiz
  已随 ADR-060 步骤 3 退役，结论不变）；用户可见
  单位与主流终端一致；性能验收也按行数断言。内存最坏 100k 行 150–200 MB 已写入
  设置注释。若将来真实痛点是宽窗口内存放大（内存 ≈ 列数 × 行数 × ≤72B/格），
  备选是内部派生守卫（按列数收紧有效行数上限），对用户不可见。
- **明确不做**：见 `mobaxterm-gap-and-priorities.md` §3 P3（本地编辑器编辑远端文件、
  命令片段、SFTP 双栏/拖放、Telnet/串口决策项、RDP/VNC/XDMCP/网络工具箱、内嵌
  X server、Mosh、`~/.ssh/config` 兼容、背景透明未立项）。
