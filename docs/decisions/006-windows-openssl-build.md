# ADR-006：Windows 构建的 OpenSSL 后端与 Strawberry Perl

状态：已接受（2026-10）。

## 背景

`ssh2` crate 的 `vendored-openssl` 会从源码编译 OpenSSL，在 Windows MSVC 目标下
Configure 阶段需要完整发行版的 Win32 perl。PATH 里命中的 Git Bash 自带 cygwin perl
缺少 `Locale::Maketext::Simple` 等核心模块，构建在 `Params/Check.pm` 处失败。

排查同时发现一个配置缺陷：libssh2-sys 在 Windows 上只有开启 `openssl-on-win32`
feature 才编译 OpenSSL 后端，否则默认退回 WinCNG。此前只开 `vendored-openssl`
时，OpenSSL 被拖进依赖图白白编译（并触发 perl 依赖），而 libssh2 实际链接的是
WinCNG——它不支持 ed25519（`wincng.h` 中 `LIBSSH2_ED25519` 恒为 0），不满足
requirements.md 的「Ed25519/RSA 私钥认证」与 ed25519 主机密钥校验需求。

跨平台参考（Ref.md 中真正内嵌 SSH 的两个项目）：

- WezTerm：libssh/libssh2 + vendored OpenSSL，官方文档要求 Windows 装 Strawberry Perl。
- Tabby：2025 年迁移到纯 Rust 的 russh 后出现服务器兼容性与 JumpServer 断连回归，
  说明纯 Rust 栈尚未完全成熟。

## 决策

跟随 WezTerm 模式：

- `ssh2` 同时开启 `vendored-openssl` 与 `openssl-on-win32`：全平台 vendored
  OpenSSL，Windows 上 libssh2 真正链接 OpenSSL 后端（ed25519 可用）。
- Windows 开发机一次性安装 Strawberry Perl（`winget install
  StrawberryPerl.StrawberryPerl`），用于源码编译 vendored OpenSSL。
- 不选 WinCNG（缺 ed25519，违反需求）；不迁移 russh（ADR-004 的 libssh2 路线
  维持不变，Tabby 的回归案例是反证）。

## 后果

- Windows 首次构建需要源码编译 OpenSSL（数分钟）；之后增量构建无额外成本。
- 新开发机（含 CI）需要预装 Strawberry Perl；NASM 不需要，openssl-src 在找不到
  nasm 时自动降级 `no-asm`。
- openssl-src 解析 perl 的顺序是 `OPENSSL_SRC_PERL` > `PERL` > PATH。当 PATH 中
  Git Bash 的 `usr\bin` 抢先命中时（本仓库所在机器即如此），可在当前终端用
  `PERL=C:\Strawberry\perl\bin\perl.exe`（cmd；PowerShell 写法为
  `$env:PERL = 'C:\Strawberry\perl\bin\perl.exe'`）钉住；Strawberry Perl 默认
  装进系统 PATH，新开的终端无需任何设置。
- Linux/macOS 无变化：vendored OpenSSL 照旧源码编译，不依赖系统开发包。

## 验证

- Windows：`cargo build`（Strawberry Perl 就位后）通过，产物链接 vendored OpenSSL。
- ed25519 路径由既有 SSH 端到端用例保护，跑法见 runbooks/ssh-e2e.md。

## 相关

- [ADR-004](004-ssh-session-transport.md)：SSH 传输层与 libssh2 路线
- [ADR-002](002-ssh-and-sftp-safety.md)：主机密钥信任与 ed25519 相关安全规则
- [runbooks/ssh-e2e.md](../runbooks/ssh-e2e.md)
