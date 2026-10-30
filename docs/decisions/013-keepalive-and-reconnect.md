# ADR-013:keepalive 心跳与断线自动重连

状态:已接受(2026-10)

## 决策

### keepalive

- 常量写死(暂不进设置界面):间隔 `KEEPALIVE_INTERVAL = 30s`,连续
  `KEEPALIVE_MAX_SILENCE = 3` 个间隔无任何入向数据判定连接死亡(约 90s)。
- 实现挂点:pump 的 `wait_budget` 增加一档"距下次心跳的剩余时间"——空闲时
  等待上限从"无限"变为"到下次心跳";心跳开启后"空闲零唤醒"退化为
  "空闲最长 30s 醒一次",这正是心跳语义。
- 发送:`session.set_keepalive(want_reply=true, interval)` 配置后,pump 到点
  调 `keepalive_send`(EAGAIN 顺延 1s 重试)。响应与一切入向数据都表现为
  socket 可读,因此"活着"的判据是 **poll 返回时 SSH socket 可读**;
  不依赖某条通道的 read 结果(keepalive 响应会被 libssh2 内部消费,
  不会出现在 shell 通道的读里)。
- 判死即 `finish_session("连接无响应")`:走现有终态路径,与 socket 断开
  同一出路。

### 自动重连

- **只对"曾经连上过"的会话生效**(收到过 `Started`,即认证成功过)。
  连接失败、认证失败(`Failed` 且从未 `Started`)不自动重试——错误口令
  反复撞服务器会触发防爆破封锁。
- 参数:指数退避 1s/2s/4s/8s/16s,最多 5 次;期间底栏显示倒计时与次数,
  **Esc 取消**,手动 R 随时立即重试(并取消自动计划)。
- 重连沿用现有"原地重连"机制(代数隔离,迟到事件不污染新会话);终端是新
  PTY,画面清空。转发规则与 agent 转发开关在目标配置里,重连自然重建;
  失败的规则照常标红(如端口被占)。
- 5 次用尽进入普通结束态(仍可手动 R)。

### 资格判定:终态必须携带原因类别(落地后修订)

- 初版把「曾经连上过」当唯一资格,`Exited` 事件一律排重连——用户在 SSH 里
  `exit` 也会 1 秒后自动爬起来,会话结束不了。根因:`ControlEvent::Exited(String)`
  只有文案,UI 无从区分「意外断线」与「用户主动结束」。
- 修订:终态事件携带 `SessionExit { description, reason }`,原因三分类
  `ExitReason::{Closed, ShellExit, ConnectionLost}`;「是否属于意外断线」的判定
  收敛在 `ExitReason::is_disconnect()` 一个定义点,UI 只对断线类排重连。
- 归类(产生点在 pump/worker 的各收尾出口):关闭命令与写端丢弃 = `Closed`;
  本地/远端 shell 正常退出、输出流 EOF = `ShellExit`;socket 读写错误、keepalive
  超时、SFTP 传输僵死强制关闭 = `ConnectionLost`。

## 后果

- pump 侧新增心跳状态(下次发送时刻、最近入向时间);`poll_with_wakeup`
  需要向调用方回报"SSH socket 本次是否可读"(等待结果原语的小扩展)。
- UI 侧 `TerminalView` 新增重连计划状态;底栏 `Ended` 形态扩展倒计时展示。
- 不持久化重连进度:重启后不自动恢复连接(需求 §7 既定边界)。

## 修订（2026-10-28）：重连策略归位 session 域（重连计划）

- 资格判定（`ExitReason::is_disconnect` × 认证成功过 × 可重连目标）此前在
  session 层、退避/上限/取消/看门狗在视图层——一条规则分居两层且零测试。
  修订：`session::reconnect::ReconnectPlanner`（重连计划，见 GLOSSARY）成为
  裁决单点，纯状态机、零 gpui 依赖，事件与时钟由视图喂入；视图只保留每秒
  时钟驱动、原地重连的 spawn、倒计时渲染与文案措辞（「已用尽/已取消」仍是
  UI 文案，planner 只返回类型化结果）。
- 看门狗（远端连接 120s 无响应视同断线）一并并入裁决单点；「本地会话不判、
  信任弹窗期间不判」从调用方纪律变成 planner 语义（视图只转述连接期事实
  `ConnectFacts`，不做判断）。
- 行为零变化：退避 1/2/4/8/16、5 次上限、首次失败不自动重试（防爆破）、
  手动 R 满额重来，全部逐条保持并由无头测试守住（矩阵 10 项：退避全链、
  用尽、资格守门、主动结束不爬起、本地不排、资格重置、Esc、R、看门狗
  三边界、到点精度与倒计时折算）。
- `TerminalView` 删除 `ever_connected`/`reconnect_attempts`/`reconnect_plan`
  字段与三个策略常量；终态事件的排计划调用改为消费 `ScheduleOutcome`。
