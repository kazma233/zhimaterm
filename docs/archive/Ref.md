# 控制台项目参考（有源码）：

> 归档（2026-10-30）：立项期选型调研。技术栈已定型（gpui + alacritty_terminal +
> 自研 `src/pty`（ADR-025）+ ssh2），文内「portable-pty 首选」等推荐与结尾的
> GUI 框架权衡（egui/iced/GPUI）是决策前笔记，仅作历史依据。

- [Alacritty](https://github.com/alacritty/alacritty)
- [Kitty](https://github.com/kovidgoyal/kitty)
- [Ghostty](https://github.com/ghostty-org/ghostty)
- [Rio](https://github.com/raphamorim/rio)
- [wezterm](https://github.com/wezterm/wezterm)
- [par-term](https://github.com/paulrobello/par-term)
- [wrap](https://github.com/warpdotdev/warp)
- [konsole](https://github.com/kde/konsole)
- [tabby](https://github.com/eugeny/tabby)
- [hyper](https://github.com/vercel/hyper)

# VT解析库（负责ANSI转义码、屏幕网格、缓冲区，**不处理进程/PTY**）
> VT只做：解析转义序列、维护字符网格、光标、颜色、回滚缓存。

1. **alacritty_terminal（Rust）**
    - 配套：`vte` crate（底层状态机）
    - 优点：**Rust生态最成熟、兼容性最好**，大量项目在用（Zed编辑器终端）
    - 缺点：PTY代码在Alacritty仓库内部，**不能单独拿出来当crate使用**；只提供VT内核
    - 适合：你只需要VT解析，PTY另外选库

2. **rio-vt（Rust，Rio Terminal拆分）**
    - 优点：独立发布crate，无渲染依赖，支持Sixel等扩展；可以搭配teletypewriter
    - 缺点：项目较新，部分边界VT序列兼容性弱于alacritty_terminal，API容易breaking变更
    - 适合：想一套代码未来移植WASM，和teletypewriter配套使用

3. **libghostty（Zig，Ghostty）**
    - 优点：完整VT实现，对外提供**C ABI**，Rust/C都可以FFI调用；自带PTY
    - 缺点：主体Zig，Rust调用要走FFI；语言0.x版本
    - 适合：跨语言嵌入终端组件

4. **par-term-emu-core-rust（Rust，par-term）**
    - 优点：VT+PTY打包在同一个crate，开箱即用，支持图片协议
    - 缺点：一体化包，不能单独拆分VT或者PTY

5. **xterm.js（TypeScript）**
    - 优点：网页终端标杆，VT兼容性强
    - 缺点：JS，**浏览器无法创建本地PTY**，只能做前端渲染，后端单独起PTY进程
    - 适合：Web终端前端界面

6. **emux-vt（Rust，emux项目）**
    - 优点：VT + 内置多路复用（类似tmux）独立crate
    - 适合：需要自带分屏/会话管理的场景

---

# PTY库（伪终端，负责拉起shell、双向字节转发、窗口resize、进程信号）
> PTY只管：创建master/slave，进程通信，**看不懂ANSI/文字**

1. **portable-pty（Rust，WezTerm）⭐推荐首选**
    - 跨平台：Linux/macOS openpty，Windows ConPTY
    - 优点：成熟稳定，独立crates.io发布，大量项目引用
    - 缺点：**只做PTY，不带VT解析**，需要单独搭配VT库

2. **teletypewriter（Rust，Rio Terminal拆分）**
    - 跨平台：openpty + Windows ConPTY，不依赖winpty
    - 优点：独立crate，API设计贴合Rio终端，和rio-vt配套
    - 缺点：较新，API会breaking升级

3. **libghostty（Zig）**
    - 自带PTY实现，C ABI对外暴露
    - 特点：VT+PTY一体，FFI调用；底层是Zig封装openpty/ConPTY

4. **nix（Rust）**
    - Unix平台底层系统调用封装，可以自己手写openpty
    - 缺点：**没有Windows ConPTY支持**，需要自己处理大量pty细节、信号、子进程管理，属于裸底层库

5. **windows-sys + libc（Rust FFI）**
    - 不是完整pty库，只是系统绑定，自己手写PTY
    - 适合学习底层原理，**不建议直接用于产品**

---

# 常用组合方案（直接抄作业）
## 方案1（稳定首选，推荐）
`portable-pty`（PTY） + `alacritty_terminal`（VT）
> 成熟，bug少；分开两个独立库，自由度最高。

## 方案2（Rio全套，想未来支持WASM）
`teletypewriter`（PTY） + `rio-vt`（VT）
> 同一套项目的两个crate，接口设计配套，支持编译WASM版本（WASM无pty）

## 方案3（跨语言嵌入，C/Rust都能调用）
libghostty（Zig，FFI）
> VT+PTY打包成动态库

## 方案4（网页终端）
后端：`portable-pty` 起PTY；前端：xterm.js做VT渲染

# 快速选型一句话总结
- 追求稳定、生产级：**portable-pty + alacritty_terminal**
- 想WASM、一套代码多端：**teletypewriter + rio-vt**
- Web前端VT渲染：**xterm.js**

---

## 一、现代 GPU 高性能终端（自研核心参考价值最高）

这类是当前技术最前沿的终端，全部采用 GPU 加速渲染，也是做自研终端的主要参考对象。

### 1. Alacritty

- **核心定位**：极简主义高性能 GPU 终端，行业性能标杆
- **技术栈**：Rust + OpenGL ES 2.0；自研`vte`状态机，封装为`alacritty_terminal`独立 crate；PTY 内部自研不对外发布；winit 做窗口管理
- **核心亮点**
  - 极致低输入延迟，海量文本滚动性能拉满，vtebench 基准测试常年处于第一梯队
  - 设计哲学极度克制，**坚决不内置标签、分屏**，主张该能力交给 tmux/zellij 等终端多路复用器
  - 支持配置热重载、Vi 模式、滚动搜索、链接点击、自动选中等实用功能
  - 跨平台覆盖 Windows/macOS/Linux/BSD，全平台一致性高
- **短板**：原生功能少，无内置分屏 / 标签；PTY 实现未独立发布，外部项目无法直接复用
- **自研参考价值**：极高。`alacritty_terminal`是 Rust 生态最成熟、兼容性最好的 VT 解析库，被 Zed 编辑器等大量项目作为终端组件复用。

### 2. WezTerm

- **核心定位**：全能型 GPU 加速终端 + 内置终端多路复用器
- **技术栈**：Rust + wgpu（WebGPU）；自研 VT 引擎；剥离出独立的`portable-pty`跨平台 PTY 库；内置完整多路复用能力
- **核心亮点**
  - 内置标签、任意分屏、会话持久化，无需额外安装 tmux 即可实现多窗口工作区
  - 原生支持 Lua 脚本扩展，可高度定制终端行为、快捷键、UI 样式
  - 完整支持 Kitty/Sixel/iTerm2 图片协议，原生集成 SSH/SFTP/ 串口连接
  - 跨平台覆盖 Windows/macOS/Linux/BSD，对 Windows ConPTY 适配完善
- **短板**：VT 引擎未拆分为独立可复用 crate；功能丰富带来的学习曲线偏陡
- **自研参考价值**：极高。`portable-pty`是 Rust 生态跨平台 PTY 的事实标准，成熟稳定，几乎是自研终端的 PTY 层首选。

### 3. Rio Terminal

- **核心定位**：新一代 WebGPU 终端，主打桌面 + WASM 网页双端统一
- **技术栈**：Rust + wgpu（WebGPU）；`rio-vt`独立 VT 解析 crate；`teletypewriter`独立 PTY crate；自研`sugarloaf`文字渲染库
- **核心亮点**
  - 架构完全组件化解耦，VT、PTY、渲染、窗口全部拆分为独立可复用的 crate
  - 支持编译为 WASM 在浏览器中运行，**一套代码同时覆盖桌面端与网页端**
  - 内置标签、分屏，支持自定义 GPU shader 视觉特效（CRT 效果、背景模糊等）
  - 支持 Sixel 图片协议、COLRv1 彩色 emoji，现代终端特性齐全
- **短板**：项目较新，冷门 VT 转义序列兼容性弱于老牌终端；版本迭代快，API 易出现破坏性变更
- **自研参考价值**：高。完整的模块化拆分方案非常值得学习，也是 WASM 终端路线的参考范本。

### 4. Ghostty

- **核心定位**：原生体验优先的高性能终端，支持组件化嵌入
- **技术栈**：Zig 语言核心；macOS 采用 Metal 渲染 + SwiftUI 原生界面，Linux 采用 OpenGL+GTK4；VT/PTY 全部封装进`libghostty`，对外提供 C ABI
- **核心亮点**
  - 坚持平台原生 UI 体验，macOS 使用系统原生设置面板、菜单栏，而非通用自绘 UI
  - `libghostty`可作为独立库嵌入任意第三方程序，支持跨语言 FFI 调用
  - 性能对标第一梯队，支持字体连字、完整 VT 标准兼容，xterm 兼容性测试表现优秀
  - 内置标签、分屏、多窗口，基础终端功能完备
- **短板**：Windows 版本仍在完善中；Zig 生态相对小众；尚未发布 1.0 正式版
- **自研参考价值**：中高。跨语言嵌入终端的典型方案，原生 UI 集成的思路值得借鉴。

### 5. Kitty

- **核心定位**：老牌高性能 GPU 终端，终端图片协议开创者
- **技术栈**：C 语言核心 + Python 配置系统；OpenGL 渲染；自研 VT/PTY，不对外拆分组件；首创 Kitty 图片协议
- **核心亮点**
  - 行业首创 Kitty 终端图片协议，现已成为主流终端广泛支持的事实标准
  - 字体连字、复杂字形渲染能力极强，对非拉丁文字支持完善
  - 内置标签、分屏、会话管理，支持丰富的终端扩展协议
  - 高度可配置，Python 插件提供了极强的扩展能力
- **短板**：Windows 平台支持偏弱；GPL3 协议限制商业二次分发；核心组件无法独立复用
- **自研参考价值**：中。终端协议规范的参考价值很高，但技术栈偏老旧，不适合作为现代 Rust 项目的复用对象。

### 6. par-term

- **核心定位**：小众新兴一体化 GPU 终端
- **技术栈**：Rust + wgpu + egui；自研 VT+PTY 打包为`par-term-emu-core-rust`单个 crate；底层 PTY 封装 portable-pty
- **核心亮点**
  - 开箱即支持 Sixel/Kitty/iTerm2 全套图片协议，无需额外开发对接
  - VT+PTY 一体化打包，单个 crate 即可搭建完整终端内核，原型开发速度快
  - 自带 Python 绑定，支持 Python 语言直接调用终端内核
  - 使用 egui 实现设置面板、覆盖层 UI，开发成本低
- **短板**：单人维护，社区体量极小；API 频繁破坏性变动；VT 与 PTY 高度耦合，无法单独替换组件
- **自研参考价值**：中。快速原型验证、图片协议集成可以参考，不建议生产环境强依赖。

### 7. Warp

- **核心定位**：AI 原生商业终端，主打块式交互与团队协作
- **技术栈**：Rust + wgpu；底层 VT 复用`alacritty_terminal`；PTY 内部封装；上层交互 UI 闭源
- **核心亮点**
  - 创新的块式命令界面，每条命令独立成块，支持折叠、复制、一键重跑
  - 内置 AI 命令生成、错误自动修复、自然语言转命令等 AI 能力
  - 团队协作功能，支持终端会话共享、云端配置同步
  - 现代 UI 设计，内置标签分屏，上手门槛低
- **短板**：上层 UI 闭源，无法完整二次开发；商业产品，高级功能需付费
- **自研参考价值**：中。AI 终端的交互设计、块式 UI 的产品思路可参考。

## 二、macOS 专属优化终端

### 8. Kaku

- **核心定位**：macOS 专属开箱即用型终端，面向 AI 编码场景
- **技术栈**：Rust，基于 WezTerm 内核二次开发；保留完整 Lua 配置体系；针对 macOS 深度优化
- **核心亮点**
  - 零配置开箱即用：自带 JetBrains Mono 字体、自动深浅色主题、Mac 原生快捷键体系
  - AI 友好设计：内置命令建议、自然语言转命令、AI 聊天面板，兼容 Claude/Copilot 等多家 AI 服务
  - 常用工具预集成：Lazygit、Yazi 文件管理器、shell 语法高亮、目录快速跳转
  - 标签导航器、会话恢复、右键菜单等细节针对 Mac 体验深度优化
- **短板**：仅支持 macOS 平台；属于二次发行版，无独立底层核心技术
- **自研参考价值**：低。产品化体验设计可参考，无底层技术参考价值。

## 三、Electron/Web 技术栈终端

### 9. Tabby

- **核心定位**：跨平台运维向终端，一站式多协议连接管理
- **技术栈**：TypeScript + Electron；基于 xterm.js 做 VT 渲染；node-pty 实现本地 PTY；支持 Web 端部署
- **核心亮点**
  - 原生集成 SSH、Telnet、串口、SFTP，无需额外工具即可管理远程服务器与设备
  - 丰富的插件生态，支持 Docker、云服务配置同步、MCP AI 集成等扩展能力
  - 内置标签、嵌套分屏、Quake 下拉终端模式
  - 支持便携版运行，配置可云端同步
- **短板**：Electron 套壳内存占用高；输入延迟明显高于原生 GPU 终端
- **自研参考价值**：低。运维功能设计、插件体系架构可参考，底层技术无参考价值。

### 10. Hyper

- **核心定位**：高颜值 Web 技术栈终端，主打高度可定制
- **技术栈**：TypeScript + Electron + React；基于 xterm.js 做 VT 渲染；node-pty 实现本地 PTY
- **核心亮点**
  - 完全基于 Web 技术，用 CSS/JS 即可定制主题和插件，扩展开发门槛极低
  - 高颜值设计风格，社区主题生态极其丰富
  - 跨平台支持 Windows/macOS/Linux
- **短板**：性能弱，内存占用高；输入延迟大，不适合高强度终端工作场景
- **自研参考价值**：低。Web 终端的 UI 定制思路可参考。

## 四、老牌桌面原生终端

### 11. Konsole

- **核心定位**：KDE 桌面默认老牌终端，功能全面稳定
- **技术栈**：C++ + Qt 框架；自研 VT/PTY；原生支持 X11 与 Wayland
- **核心亮点**
  - KDE 桌面深度集成，与 Dolphin 文件管理器、系统通知、全局快捷键联动顺畅
  - 功能全面：标签、分屏、多配置文件管理、多会话切换
  - 稳定成熟，是 Linux KDE 桌面用户的默认选择
- **短板**：渲染性能远低于 GPU 终端；技术栈老旧；核心组件无法独立复用
- **自研参考价值**：低。仅适合学习老牌终端的功能设计。

---

## 行业内 4 种典型 UI 实现策略

对应上面不同终端的路线，本质就是 4 种选择：

### 方案 A：极致性能路线（完全自绘，不用 UI 框架）

- 代表：Alacritty、WezTerm、Rio、Kitty
- 做法：所有视觉元素全部 GPU 手动画，连滚动条、右键菜单都自己写。
- 优点：输入延迟最低、性能最强、完全可控。
- 缺点：开发量大，复杂界面（设置面板）开发成本高。

### 方案 B：平衡路线（核心自绘 + 轻量 UI 框架做覆盖层）

- 代表：par-term
- 做法：终端核心文字自绘（性能敏感区）；设置面板、命令面板这种低频复杂 UI 用 egui。
- 优点：开发效率和性能兼顾，是目前中小型项目性价比最高的路线。

### 方案 C：原生体验路线（系统原生 UI 框架）

- 代表：Ghostty、macOS 原生 Terminal
- 做法：终端渲染自绘，外围界面用平台原生控件（SwiftUI / GTK）。
- 优点：平台体验最好，和系统无缝衔接。
- 缺点：跨平台一致性差，每个平台要单独写一套 UI。

### 方案 D：快速开发路线（全 UI 框架 / Web 技术栈）

- 代表：Konsole、Tabby、Hyper
- 做法：全程用现成 UI 框架，终端区域当一个控件嵌进去。
- 优点：开发最快，控件生态完善。
- 缺点：性能最差，输入延迟高，不适合高强度终端场景。

考虑用下GUI框架，因为我们不是真的做一个强悍的终端，还需要ssh管理和sftp能力，后面可能继续扩展：
- egui + egui_wgpu
- iced + iced_wgpu
- GPUI + wgpu