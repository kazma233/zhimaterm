# 调研：要不要把 smol 换成 tokio

> 归档（2026-10-30）：结论（现在不换；未来引入 tokio 原生库时按「边界共存」接入）
> 已由 `../context/requirements.md` §2 承接。「事实」节为 2026-10 快照——当时 PTY
> 走 portable-pty（现为自研 `src/pty`，ADR-025）、gpui-pre 为 0.3.7（现 0.3.10）。

状态：结论——现在不换；未来引入 tokio 原生库时按「边界共存」接入。

## 背景

排查「连接失败事件丢失」时一度怀疑 smol 的 `future::or` 组合语义，由此提出是否
值得整体迁移到 tokio。最终定位的根因是 `SessionEvents::recv` 对「单侧通道关闭
返回 None」的处理错误（已修复并有回归测试保护），与运行时无关，但迁移评估
仍然值得记录。

## 事实

- gpui-pre 0.3.7 全家依赖里没有 tokio / async-std / smol：gpui 用自研 executor
  （Zed 官方立场即不用 tokio），`cx.spawn` 的 future 由它驱动。
- zhimaterm 的 smol 使用面：`block_on` ×58、`future::or` ×18、`Timer` ×16、
  `channel` ×若干。分布广但 API 面窄（四类）。
- 当前依赖树没有 tokio 需求：SSH 走 ssh2/libssh2（同步 C 库）、PTY 走
  portable-pty、加密走 rustls 无关的 OpenSSL。

## 不换的理由

1. **本次 bug 换运行时修不了**：`or` 的「任一分支完成即返回」语义在 tokio 的
   `select!` 里完全对应（分支 recv 返回 None 同样要显式 disable 分支），
   语义正确性必须自己写对——与用哪个运行时无关。
2. **混合运行时的真实成本**：tokio 的 time/net 必须在 tokio context 内跑，
   与 gpui executor 共存需要全局 Runtime + 两套 waker 互通；58 处 block_on
   的线程驱动方式、18 处 or、16 处 Timer 全部要改。改动面 = 整个会话层。
3. **收益对不上成本**：tokio 的价值在生态库，当前没有要接的 tokio 原生库。

## 何时值得、以及正确姿势

引入 russh（tokio 原生 SSH）、tonic/gRPC、tokio-tungstenite 等库时：
**起一个专用 tokio Runtime 跑这些库，与 gpui executor / 会话 worker 之间用
通道桥接**（worker 仍是自己的线程 + 同步 pump），不全局替换 smol。
届时新写代码优先 tokio，存量 smol 不动——两套在各自边界内并存。

## 相关

- [ADR-004](../decisions/004-ssh-session-transport.md)：会话层线程模型（迁移的主要影响面）
