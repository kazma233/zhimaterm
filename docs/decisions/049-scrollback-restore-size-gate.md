# ADR-049：回滚恢复的尺寸信任与不变式守护

状态：已接受（2026-10-30）。修订 ADR-046 的恢复口径：超时兜底尺寸此后只
决定 PTY 出生尺寸，不再承载回滚灌入。

## 背景

ADR-046 把「恢复发生在什么尺寸上」定为显式契约：等首个 Resize 再开工。
但等待有上限（250ms），超时按 fallback 96×30 开工并**照常灌入回滚**。
实测（2026-10-30，恢复后样式错乱）发现前提破灭时动作照做，两条症状：

1. **非活跃 tab 的会话永远等不到首帧 Resize。** Resize 在 paint 阶段的
   canvas 回调里发出（`terminal_view`），而只有活跃 tab 参与布局。工作区
   恢复出的非活跃 tab 会话 100% 走 fallback 灌入；用户切过去的那个 Resize
   （行数+列数同时变）触发 alacritty `grow_lines` 把灌入历史的内容吸回
   视口——分界行与提示符钉在屏幕中部，视口从内容中间开始。
2. **fallback 列宽灌入把满宽背景行折成两段。** 列数修正的 reflow
   （`grow_columns`）沿 WRAPLINE 接回时产生「满宽背景行」与「纯背景残段」
   交错——zcode CLI 命令块的背景被撕成条纹。

模型层实验（96×30 恢复 → resize(120,40)）稳定复现两者：history 从 64 掉
到 38，视口被恢复内容占据，背景行与 72 格残段交错。

根因不是 resize 行为（那是所有终端的标准语义），而是**灌入没有被约束在
「尺寸可信」的前提下执行**：250ms 的超时把「等不到」悄悄当成了「等到了」。

## 决策

1. **尺寸信任是显式信号**：`wait_startup_size` 的产出带 `size_trusted`——
   `Fixed` 与限期内到达的 Resize 为可信；超时兜底不可信。
2. **灌入只发生在可信尺寸上**：可信 → 开工即灌（现状）；不可信 → 内容
   读出在手（`scrollback::read`），挂进 `RestoreHold::Pending`，不灌。
3. **Pending 在首个 Resize 应用后灌入**（`apply_command` 的 Resize 分支，
   `terminal.resize` 之后、SIGWINCH 发出之前）：同帧多条 Resize 天然按最终
   尺寸折行，列数从一开始就对。fallback 期 shell 打出的内容随滚入技巧
   归档进历史；SIGWINCH 的重绘落在清屏后的空屏上。
4. **Armed 窗口守护不变式**：灌入后、子进程首字节前，行数增长的 resize
   先清屏清历史（CSI 2J/3J/H）再按新尺寸重灌。这个窗口里视口与历史只
   可能含恢复内容，整备无损失、无需区分内容归属。首字节到达即关闭窗口：
   之后的 resize 是用户行为，吸回历史是标准语义，不干预。
5. **未灌入不写盘**：`Pending` 期间空闲写回与退出写盘都跳过（与写失败同
   样退避，避免等待归零空转）——回滚文件只能被已含恢复内容的模型覆盖。

## 备选方案

- **吸回后用 Scroll::Delta 抵消**：display_offset 只挪视口窗口，不回移
  网格数据，光标与提示符照样错。现象补丁，否决。
- **给恢复内容打标记、resize 后识别重滚**：标记无法跟随 reflow，内容
  归属判定边界模糊（ADR-046 已否决同类）。否决。
- **非激活格推迟 open PTY**：改变「切过去就已在」的工作区恢复语义
  （ADR-046 备选 3），不翻案。
- **取消超时无限等 layout**：layout 不来可能是 UI 异常，worker 必须有
  恢复边界；本方案把超时的后果收窄为只管 PTY 出生尺寸，不需要取消它。

## 后果

- 恢复格的可见契约（内容进历史、提示符从空屏顶部开始）在所有到达路径
  上成立，包括非活跃 tab 首次激活。
- 未灌入期间关闭会话：回滚文件保持上一份原样，下次启动仍可恢复。
- 非激活格的 PTY 出生尺寸仍是 fallback（真实尺寸在激活前不存在）；
  inline TUI 按 SIGWINCH 重绘自愈，与现状一致，不属于本契约。
- 灌入瞬间之后、首字节之前的跨帧布局抖动若与 shell 重绘输出竞争，残余
  1~2 行吸回，与用户手动 resize 同类，按标准语义豁免。

## 验证

- 单元：`size_trust_follows_the_size_source`、`restore_hold_pours_on_the_
  first_resize`、`restore_hold_repours_on_growth_only`、
  `restore_hold_closes_and_gates_persist`（worker）；
  `clearing_and_repouring_restores_the_contract_after_growth`（模型层，
  守住吸回与背景撕裂两个分区）。
- e2e：`restored_scrollback_waits_for_a_late_resize`——兜底开工空窗跨过
  2 秒空闲写回周期，回滚文件不被覆盖；首个 Resize 后内容按真实宽度进
  历史、视口干净。
- 手工：双 tab 工作区恢复后直接切第二个 tab（提示符在顶部、无条纹）、
  关闭从未激活的 tab（回滚文件不变）、激活后手动拖窗口（标准吸回）。
