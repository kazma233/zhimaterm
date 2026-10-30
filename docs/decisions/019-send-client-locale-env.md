# ADR-019:SSH 会话向远端发送 LANG 环境变量

状态:已接受(2026-10)

## 背景

乱码排查插队项结论(2026-10-16,优先级文档):用户看到的「控制台中文乱码」是
docker 测试服务器没有 locale(POSIX),GNU `ls` 把合法 UTF-8 中文文件名八进制
转义成 `$'\351\232\220...'`。促成因素:zhimaterm 建 shell channel 时不发
任何环境变量,而 OpenSSH 客户端默认 `SendEnv LANG LC_*`(macOS
`/etc/ssh/ssh_config`)——配了 `AcceptEnv` 的服务器(Debian 系默认)会继承
客户端 locale。zhimaterm 与系统 ssh 的行为差异由此而来。

## 决策

### 时机与范围

shell channel 建立序列为 pty →(agent 转发)→ env(LANG)→ shell,顺序
对齐 OpenSSH 客户端。只发 `LANG` 一条——显示编码场景由它覆盖,逐项发送
`LC_*` 全套是 OpenSSH 的历史包袱,不跟进。SFTP 与监控的 exec 通道不发。

### 值推导(`client_lang`,进程内缓存一次)

1. 应用进程 env 的 `LANG` 非空则直接用它(终端里 `cargo run`、继承 shell
   环境的场景)。
2. macOS 上无 `LANG` 时读 `defaults read -g AppleLocale`(Finder/Dock 启动
   的 GUI 进程没有 LANG;Terminal.app 同样用 AppleLocale 构造 LANG),剥掉
   `@` 修饰(如 `en_US@rg=...`),无编码后缀则补 `.UTF-8`。
3. 推不出(Windows GUI 等场景)就不发,维持服务器自己的默认 locale——与
   OpenSSH「客户端有什么发什么」语义一致。

### 尽力而为,拒绝不阻断

libssh2 的 env 请求以 want_reply 发送并等回包,服务端未配 `AcceptEnv` 会答
拒绝,错误就地吞掉:不中断连接、不提示(OpenSSH 客户端被拒同样无感)。请求用
独立的 5 秒预算(`LANG_ENV_BUDGET`),病态服务器不回包时只损失 locale 下发,
不侵占 shell 的连接预算,连接继续。

### 底栏 LANG 状态角标(2026-10-16 修订)

原「拒绝不提示」修订为「不弹窗不打断,状态进底栏」:下发结果连值封装为
`LangStatus`,经 `ControlEvent::LangStatus` 随会话启动送达 UI(Transport 的
attach/take 模式,worker 在 `Started` 后发射)。底栏信息区尾部展示
`LANG <值>`,颜色承载结果:青绿=远端接受,红=被拒/失败(用户口径)。客户端
推不出 LANG 时不发事件、无角标;已结束会话的底栏不展示。角标是被动状态,
不是告警——被拒不阻断任何功能,与监控「不可用」的展示同级。

## 后果

- 配了 `AcceptEnv LANG LC_*` 的服务器上,会话 locale 与客户端一致,UTF-8
  文件名与中文输出直接可见;乱码插队项的转义在这类服务器上消失。
- 服务器缺对应 locale(如容器无 `zh_CN.UTF-8`)时 shell 打一行 setlocale
  告警,`LANG` 仍进入环境,不影响连接。
- 未配 `AcceptEnv` 的服务器(如 2222 测试端)行为不变:请求被拒、被吞。
- 每次连接多一个 env 请求往返,与 OpenSSH 同量级。
- macOS 首次推导 spawn 一次 `defaults` 子进程,结果进程内缓存。
- 本地 shell 面板存在同类问题(GUI 启动无 LANG,本地 `ls` 也会转义),
  不在本 ADR 范围,发现于乱码排查,待另行立项。
