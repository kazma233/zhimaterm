# 第三方软件声明

ZhimaTerm 自身以 [MIT](LICENSE) 许可发布。本文件记录两类第三方软件：**复制进本仓库
源码的代码**（必须随源码保留原版权声明）与**编译进发布二进制的依赖**。

分发二进制时，本文件需与 `LICENSE` 一并提供。

## 1. 复制进本仓库源码的代码

### 1.1 MSVCRT 命令行引用规则

- 位置：`src/pty/win.rs` 的 `append_quoted`
- 来源：portable-pty 0.9.0 的 `pty/src/cmdbuilder.rs`，其代码又来自
  [rust-subprocess](https://github.com/hniksic/rust-subprocess)（`src/popen.rs`），
  再往前是 ArgvQuote（<http://tinyurl.com/zmgtnls>）
- 改动：仅函数外壳（portable-pty 中是方法，本项目是自由函数）与缩进；逻辑逐字一致
- 理由：本项目的 Windows PTY 走自研实现（ADR-025），但 Windows 命令行的
  MSVCRT 引用规则与 portable-pty 一致，直接沿用其已验证的实现

portable-pty 与 rust-subprocess 均为 MIT 许可，需保留原版权声明与许可全文：

```
MIT License

Copyright (c) 2018 Wez Furlong

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

```
Copyright (c) 2017 Hrvoje Nikšić

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the
Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE
WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE
AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
USE OR OTHER DEALINGS IN THE SOFTWARE.
```

### 1.2 语义参考（非逐字复制）

`src/pty/` 整体是自研实现（ADR-025），语义对齐 portable-pty 0.9.0，未复制其源码。
对齐口径记录在 `docs/decisions/025-selfhost-pty.md`，逐条差异也在该文中列明。

## 2. 编译进发布二进制的依赖

### 2.1 构建期 vendored 的 C 库

| 组件 | 版本 | 许可证 | 归属 | 来源 |
|---|---|---|---|---|
| OpenSSL | 3.6.3 | Apache-2.0 | The OpenSSL Project | 经 `openssl-src` 300.6.1，Windows 上 vendored 构建（ADR-006） |
| libssh2 | 1.11.1 | BSD-2-Clause | libssh2 项目（COPYING 中的各版权人见下） | 经 `libssh2-sys` 0.3.3 随 crate 分发 |

libssh2 的 BSD-2-Clause 原文版权行（`libssh2/COPYING`）：

```
/* Copyright (C) 2004-2007 Sara Golemon <sarag@libssh2.org>
 * Copyright (C) 2005,2006 Mikhail Gusarov <dottedmag@dottedmag.net>
 * Copyright (C) 2006-2007 The Written Word, Inc.
 * Copyright (C) 2007 Eli Fant <elifantu@mail.ru>
 * Copyright (C) 2009-2023 Daniel Stenberg
 * Copyright (C) 2008, 2009 Simon Josefsson
 * Copyright (C) 2000 Markus Friedl
 * Copyright (C) 2015 Microsoft Corp.
 * All rights reserved.
```

OpenSSL 采用 Apache-2.0，与本项目的 MIT 许可兼容。

### 2.2 UI 框架

| 组件 | 许可证 | 归属 |
|---|---|---|
| gpui（经 `gpui-pre` / `gpui-pre-platform` 0.3.10 发布） | Apache-2.0 | Copyright 2022 - 2025 Zed Industries, Inc. |

`gpui-pre` 是 [Zed](https://github.com/zed-industries/zed) 的 gpui 快照（上游 commit
`08cbb88`），以独立 crate 形式发布于 crates.io，许可与上游同为 Apache-2.0
（上游许可文件：`crates/gpui/LICENSE-APACHE`）。

### 2.3 直接依赖的 crate

许可证取自 crates.io 上各 crate 的元数据；`MIT OR Apache-2.0` 表示双许可，
本项目按 MIT 使用。

| crate | 版本约束 | 许可证 | 用途 |
|---|---|---|---|
| alacritty_terminal | 0.26.0 | Apache-2.0 | VT 状态机与 ANSI/VT 序列解析 |
| anyhow | 1 | MIT OR Apache-2.0 | 错误上下文 |
| chrono | 0.4 | MIT OR Apache-2.0 | 时间戳 |
| keyring | 3 | MIT OR Apache-2.0 | OS 安全凭据存储 |
| libc | 0.2 | MIT OR Apache-2.0 | Unix 系统调用绑定 |
| libssh2-sys | =0.3.3 | MIT OR Apache-2.0 | libssh2 的 FFI 绑定 |
| gpui-pre | =0.3.10 | Apache-2.0 | 桌面 UI 框架 |
| gpui-pre-platform | =0.3.10 | Apache-2.0 | UI 框架的平台后端 |
| regex | 1 | MIT OR Apache-2.0 | 终端内搜索的正则模式 |
| smol | 2 | Apache-2.0 OR MIT | UI 侧异步设施 |
| serde | =1.0.229 | MIT OR Apache-2.0 | 配置与存档序列化 |
| serde_json | =1.0.151 | MIT OR Apache-2.0 | JSON |
| ssh2 | =0.9.6 | MIT OR Apache-2.0 | libssh2 的安全封装 |
| unicode-segmentation | 1 | MIT OR Apache-2.0 | 字素簇切分 |
| unicode-width | 0.2 | MIT OR Apache-2.0 | 组合文本按 wcwidth 折算占格 |
| uuid | 1 | Apache-2.0 OR MIT | 保存连接条目的唯一 id |
| socket2（仅 Unix） | 0.5 | MIT OR Apache-2.0 | 监听 socket 的 SO_REUSEADDR |
| objc2（仅 macOS） | 0.6 | MIT | AppKit 类方法调用 |
| objc2-app-kit（仅 macOS） | 0.3.2 | Zlib OR Apache-2.0 OR MIT | 标准标题栏高度实测 |
| objc2-foundation（仅 macOS） | 0.3.2 | MIT | NSGeometry |
| windows-registry（仅 Windows） | =0.6.1 | MIT OR Apache-2.0 | WSL 发行版枚举 |
| windows-sys（仅 Windows） | =0.61.2 | MIT OR Apache-2.0 | ConPTY、ToolHelp、IpHelper 等 |
| winresource（仅 Windows，构建期） | 0.1 | MIT | PE 资源段嵌图标 |

完整版本见 `Cargo.lock`。

## 3. 图标

`icons/` 下的图标（`icon.png`、`icon-1024.png`、`icon.icns`、`icon.ico`）为本项目
自有资源，随源码以 MIT 许可发布。
