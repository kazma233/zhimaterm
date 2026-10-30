# ADR-030:macOS 打包——本地网络权限声明与稳定签名身份

状态:已接受(2026-10)

## 背景

打包后的 ZhimaTerm.app(用户级安装到 ~/Applications)访问局域网地址失败:内置
SSH/SFTP 连接、以及**本地 shell 里跑的 curl 等命令**,对局域网 IP 均为 1–2ms
瞬断且报 `No route to host`(curl errno=65,EHOSTUNREACH);同一时刻同机其他
进程访问同一地址正常。系统设置里从未弹过授权框。

根因是 macOS 15 起的本地网络隐私(Apple TN3179),三条口径拼起来才是全貌:

1. **触发面**:对局域网地址(192.168/10.x/172.16–31/169.254/`.local`)发起
   TCP 连接就需要本地网络权限;连公网地址不涉及。从 app 的本地 shell 里跑
   `ssh`/`curl` 也算——TN3179 的 CLI 自动放行只覆盖 "run from Terminal or
   over SSH" 的场景,从第三方 GUI app spawn 的子进程按 **responsible code**
   归因到 app 本身("if your app spawns a helper tool … macOS considers the
   app to be the responsible code")。即:内置 SSH/SFTP(进程内 ssh2 直连)与
   shell 子进程的局域网访问,统一走 ZhimaTerm 的授权。
2. **授权弹窗与拒绝形态**:对 TCP 连接被拒时系统按 EHOSTUNREACH 立即返回,
   不留可读日志(实测 networkd/nehelper 级日志监听为空);弹窗出现时**当次
   连接已被拒绝**,下一次连接才生效。
3. **身份追踪**:本地网络授权按代码签名身份绑定。链接器给 Rust 产物的
   ad-hoc 签名每次重编 identifier/UUID 都变,授权落在旧构建身上,新构建被
   **静默**拒绝且不再弹窗——设置里的开关看着是开的,对当前二进制不生效。

## 决策

### Info.plist:只加说明串,不加 Bonjour 表

`packaging/Info.plist` 增加 `NSLocalNetworkUsageDescription`(授权弹窗里向用户
说明用途,中文一句话)。**不**加 `NSBonjourServices`:TN3179 规定它只服务于
Bonjour 注册/浏览,项目无任何 mDNS/zeroconf 用法。若日后做局域网主机发现类
功能,需连同本决策一起重审。

### 打包末尾以固定身份 codesign

`install-darwin` 在组装完 .app(Info.plist 落盘之后)执行
`codesign --force --sign "{{.SIGN_IDENTITY}}" "$app"`;`SIGN_IDENTITY` 是
Taskfile 顶层变量,填签名身份名(本地自签证书的名字因机器而异,不在文档里写死),
留空则退回 ad-hoc、不报错。
要点:

- 证书是**自签代码签名证书**(钥匙串访问 → 证书助理 → 创建证书:身份类型
  自签名根证书、证书类型代码签名)。签名身份稳定 = Designated Requirement
  稳定 = 授权跨重编译/重装持久。
- 不加 `--options runtime`(硬化运行时是公证分发才需要);无 entitlements。
- TN3179 建议 Apple 签发身份最可靠。免费 Apple Development 证书(Xcode 生
  成,一年过期)可作替代;分发场景(Developer ID + 公证)不在当前范围,届时
  重审。
- 同批修正 Taskfile 两处既有缺陷,是 codesign 步骤跑得起来的前置:`VERSION` 原为
  静态字符串,`{{.VERSION}}` 展开成 grep 命令
  文本本身而非版本号,改 `sh:` 动态求值;`install-{{.OS}}` 误把 go-task 的模板
  函数 `OS` 当变量引用,展开为空、任务名不存在,改 `{{OS}}`。

### 授权恢复口径(用户操作,可复用)

设置 → 隐私与安全性 → 本地网络 → 移除 ZhimaTerm 条目 → ⌘Q 完全退出并重开
app → 访问一次局域网地址触发弹窗 → 允许 → **再发一次连接**(弹窗当次必失败,
见背景 2)。

## 排障口径(怎么确认是这类问题)

- 症状:对局域网地址连接 1–2ms 即失败,无 `Connection refused` 字样,
  errno=65;`connect_ex` 可直接取到内核返回码。
- 对照:同机同刻从 app 外访问同一地址(排除服务端);防火墙关闭、无 VPN、
  无系统扩展(逐一排除)。
- 不可行的路,别再走:`tccutil reset LocalNetwork <bundle>` 在 macOS 27 按
  包名重置不可用(exit 70);统一日志里查不到这类拒绝的记录,日志监听取证
  无效。设置列表里同一 app 出现多条重复条目(实例中 Chrome 有三条)是正常
  现象,佐证系统按身份累积条目。

## 后果

- 2026-10-19 实机解决:SIGN_IDENTITY 签名 + 移除条目重新授权后,局域网
  访问恢复(用户确认),安装包已验为真证书签名(非 adhoc)。
- 此后重编译、重装不再丢授权;首次换签名身份时需按上面口径重新授权一次。
- 本决策只覆盖 ZhimaTerm 自身;其他应用引入局域网功能时再按本决策套用。
- **unverified**:自签证书的长期表现。TN3179 明示 Apple 签发身份才最可靠,
  若日后再现"开关开着却静默拒绝",优先换 Apple Development 证书再排障。
