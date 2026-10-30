# ADR-038：动态 SOCKS 转发与运行中增删规则

状态：已接受（2026-10-21）。补齐 ADR-011 的两项显式后置。

## 背景

ADR-011 首版只做 L/R，SOCKS 与运行中增删都记为后置：规则随连接配置一次性
建立，重连时按配置重建；转发浮层纯展示，增删要去连接表单。实际使用里
「临时开条隧道」是常见动作，改配置再重连的代价太高。

## 决策

1. **规则带稳定 id**（`ForwardId`）。运行中删除会让列表下标漂移，事件与 UI
   状态一律按 id 对位；`ForwardEvent.rule` 从下标改为 id。id 在一条连接的
   生命周期内唯一：配置里的规则按顺序分配 0..n，运行中新增取「现有最大值 + 1」。
2. **SOCKS5 仅 CONNECT、无认证**（RFC 1928）。地址类型支持 IPv4 / 域名 / IPv6；
   域名**不在本地解析**，原样交给 `direct-tcpip` 由远端解析（与 `ssh -D` 一致，
   避免 DNS 泄漏）。BIND / UDP ASSOCIATE / SOCKS4 明确拒绝（REP=0x07/0x08）。
3. **握手与通道分离，成功应答在通道打开之后**：每条 SOCKS 连接一个短命线程
   做握手（阻塞读、10 秒超时），读到目标后把 `(stream, host, port)` 交给 pump；
   pump 调用 `direct-tcpip` 成功后才把成功应答写回客户端，失败回 REP=0x01。
   先回成功再开通道会让客户端在失败时看到假成功。
4. **本地监听改为 pump 内的非阻塞 accept**（L 与 D 都走这条）。原先每条规则
   一根阻塞 accept 线程，删除规则时线程仍卡在 `accept()` 里，端口要等到下一次
   连接才会释放；改为 listener fd 进等待集合、pump 每轮 accept 后，删除规则
   drop listener 即释放端口，也不再需要 accept 通道与额外唤醒源。
   Unix 在 bind 前设 `SO_REUSEADDR`（socket2）：删掉规则后马上用同一端口重建
   时，旧连接的 TIME_WAIT 不应该挡住监听。Windows **不设** `SO_REUSEADDR`
   （它允许其他进程抢占同一端口，是安全倒退），保留默认独占语义，端口仍被
   占用时由 `ListenFailed` 如实上报。
5. **运行中增删的命令链**：`Session::add_forward/remove_forward` →
   `SessionCommand` → worker → `Transport` → `PumpCommand::Forward` →
   pump 命令循环 → `ForwardSet::apply`。与设置类命令不同，这里**不静默丢弃**
   （队列满返回 `CommandError`）：用户明确要求的规则丢失没有下一次自然覆盖。
   删除会关闭该规则的本地/远端监听，并断开它在途连接（drop channel）。
6. **浮层内增删**（ADR-011 的「纯展示」随之失效）：每条规则行尾 ✕ 删除；
   底部「＋ 规则」打开一个表单模态（ADR-026 实体化），类型 chip 循环
   L → R → D，SOCKS 隐藏目标字段。连接表单的规则编辑同步支持 D。
7. **回写保存连接**：浮层底部「同时保存到连接」勾选，默认开。勾选且当前会话
   匹配一条保存的连接（host/port/user）时，增删立即用当前规则表覆盖该条
   连接的转发配置；未匹配的临时连接勾选不可用（显示「规则只对本次会话生效」），
   不把临时隧道悄悄写成永久配置。
8. **SOCKS 绑定地址**：默认 `127.0.0.1`，允许改成任意地址（与 L 转发一致）；
   非回环绑定时表单给出「对外网开放」警示。无认证监听在非回环地址等于对
   局域网开放代理，风险必须可见。

## 已知边界

- SOCKS5 无认证且无连接数上限：与 L 转发同一暴露面，按用户选择。
- 回写的 `SavedForward` 不保存 id：保存的连接按列表顺序，下次连接时重新
  分配 id。id 只在运行期有意义。
- 握手线程超时 10 秒；客户端不发完请求只占一条线程，不占 pump。

## 验证

- `cargo test forward::`：域名 CONNECT 解析、BIND 拒绝（REP=0x07）、无可用
  方法回 0xFF 后不读请求、成功应答格式。
- `cargo test -- --ignored`（需 sshd，见 runbook）：
  `forwards_can_be_added_and_removed_while_running`（新增规则立即监听并承载
  banner；删除后端口不可连）、`socks_forward_carries_the_remote_banner`
  （SOCKS 握手 + 成功应答 + 真实流量）。
- 手工：SSH 会话连上 → 浮层「＋ 规则」加一条 L → 状态变「监听中」→
  `nc` 连入 → ✕ 删除 → 端口立即拒绝；勾选「同时保存到连接」后重开连接表单
  确认规则已写入。
