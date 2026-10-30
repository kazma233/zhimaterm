# ZhimaTerm

ZhimaTerm 是面向开发者的 Rust 桌面终端应用，目标平台为 macOS、Linux 和 Windows。提供本地 Shell、SSH 远程终端、多标签会话与分屏，以及与 SSH 连接关联的 SFTP 文件浏览和传输。

## 快速开始

```sh
cargo run --bin zhimaterm   # 开发运行（Rust ≥ 1.98，工具链由 rust-toolchain.toml 固定）
task install                # 构建 release 并安装到当前用户（需 Task）
```

Windows 首次构建需从源码编译 vendored OpenSSL，先安装 Strawberry Perl，
其余构建、测试与安装细节见[开发指南](docs/context/development.md)。

## 功能特点

**终端**

- 多会话（左侧栏）与分屏（标签内二分嵌套，每标签最多 8 格）；关闭未结束的 SSH 会话或全屏程序前有二次确认。
- ANSI/VT 状态机、按样式 run 的渲染：粗体/斜体/删除线/五种下划线样式与下划线颜色/隐藏文本。
- 键盘与输入法输入（含 F1–F12/Insert、kitty disambiguate）；中文输入法组合文本直接画在光标格上。
- 鼠标选区（双击按词、三击按行、Alt 矩形块选）与鼠标上报（SGR/传统编码）、备用屏滚轮翻页（1007）。
- 超链接：OSC 8 与纯文本 URL 识别，⌘/Ctrl + 点击打开。
- 终端内搜索滚动历史（⌘F，大小写智能、可选正则）。
- 滚动历史回看、bracketed paste、同步输出（2026）、终端查询应答（DA1/DSR/DECRQM/颜色/尺寸）。
- Shell 标题与响铃、指针形状、光标形状与闪烁（DECSCUSR）。

**会话**

- 三种传输后端：本地 Shell、WSL 发行版会话（Windows）、SSH 交互 Shell；每个格是独立 worker，会话状态互相隔离。
- SSH 认证信息叠加（agent/私钥/密码，按序尝试、逐项报错）与连接期密码询问：已存凭据自动应答，临时密码不落盘。
- 跳板机/ProxyJump（单级，两次主机信任确认）；本地/远程/动态 SOCKS5 端口转发，转发浮层可运行中增删规则；agent 转发。
- keepalive 心跳与断线自动重连（指数退避、可取消）。
- 会话输出日志：只记输出不记输入，记录终端渲染后的行文本，会话内运行时开关。
- 本地默认 Shell 格的进程探针侧边栏：进程树与 LISTEN 端口，行可点开详情、可结束占用进程，开即采、关即停。

**SFTP（与 shell 共用一条 SSH 连接）**

- 目录浏览、上传/下载（单文件与整棵目录树）、重命名、删除（含递归删除）、新建目录。
- 覆盖先确认；上传/下载走临时文件 + rename 提交；传输任务可取消，结束的操作落操作记录（底栏「历史」）。

**外观与持久化**

- 主题：33 套内嵌主题，点击即换肤全界面（含终端配色与 UI），UI 颜色从终端配色自动推导。
- 外观与终端设置：终端字体（等宽候选）与字号、滚动历史上限、界面字体与字号（按 rem 整体缩放）、默认本地 Shell 与工作目录。
- 窗口几何与本地标签/分屏布局持久化（`workspace.json`，SSH 格不恢复）；本地格重启后恢复上次工作目录与最近 1,000 行带颜色/样式的回滚。

各功能的实际行为与交互细节见[功能行为说明](docs/context/features.md)。

## 文档

需求、决策、验证记录的完整目录见 [docs/README.md](docs/README.md)。
入口：[需求规格](docs/context/requirements.md)、[开发指南](docs/context/development.md)、[术语表](GLOSSARY.md)。

## 许可

[MIT](LICENSE)
