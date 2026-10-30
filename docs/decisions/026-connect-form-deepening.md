# ADR-026：连接表单深化——自持建造与编辑态

状态：已接受

## 背景

ADR-007 确立分层后，连接相关的每个 feature（ADR-011/018/020）都把一部分表单知识
留在了外壳：`ConnectForm::new` 收调用方建好的 11 个输入实体（占位文案在应用层），
约 10 个状态字段全部 `pub`，`ModalActions` 长到 17 个回调（其中 6 个开关是零逻辑
转发），8 个一行 toggle 方法挂在外壳上。`connect_saved` 的转发草稿重建也被迫留在
app.rs——注释写明接缝错位：`load_saved` 只有 `&mut App` 建不了带监听的实体。
结果是每加一个连接字段要动 5 类编辑点，app.rs 从重构后的约 860 行回到 2098 行，
其中连接领域约 400 行；dirty/合并/载入往返只能靠手工验证。

对照 `settings::form`：它收紧了下拉交互（表单方法），但实体建造同样留在应用层。

## 决策

1. **表单是实体，自持建造**。`ConnectForm::new(font, cx)` 在 `Context<Self>` 里
   自建全部输入实体，占位文案随迁入 connect.rs；持久化字段的编辑监听挂进表单
   （口令字段不影响主按钮语义，不监听），外壳只 observe 表单实体一次。
2. **字段全私有，取数走两个类型化出口**：`saved_entry(cx)`（保存路径，原有）与
   `connect_target(cx)`（连接路径：目标 + 认证 + 转发 + 跳板一次装配，任一不完整
   报错）。不引入通用的中间草稿结构——两个消费方的投影与校验本来就不同。
3. **回调集合只留语义动作**：逐控件开关经 `ConnectToggle` 枚举、转发草稿编辑经
   `ForwardEdit` 枚举各走一个分发回调，`ModalActions` 17 → 9。
4. **表单领域逻辑归表单**：`expand_host`（粘贴串拆分，含冲突报错）与
   `load_saved`（含转发草稿重建、钥匙串只读回填、载入后的焦点决策）是表单方法；
   表单返回焦点目标，`window.focus` 仍由外壳执行（焦点是窗口资源）。
5. **副作用留外壳**：钥匙串写/删与落盘（`store.upsert`）的编排——ADR-018 的
   「钥匙串先动、成功才落盘」顺序——留在 app.rs，表单不持有 store；钥匙串只读
   回填（`keychain::load`）留在 `load_saved` 内。
6. **测试接缝**：纯逻辑（拆分/并入/字段校验）保持纯测；实体级行为（载入↔保存
   往返、dirty 判定、expand_host 分支）在 connect.rs 内用 `#[gpui::test]`
   （`TestAppContext`，经 dev-dependency 的 `test-support` feature 开启，release
   不带）测试。测试里 `remember_password` 刻意保持关闭，避免触碰真实钥匙串。

## 后果

- 加连接字段只改 connect.rs（此前 5 类编辑点：表单字段/建实体/toggle 方法/
  ModalActions/装配）；开关只加 `ConnectToggle` 变体。
- app.rs 净减约 280 行；每帧 ModalActions 的 `Rc` 分配从 17 降到 9。
- 新表单一律按本 ADR 纪律建造；存量 `settings::form` 已于 2026-10-21 对齐
  （`SettingsForm::new(CandidateLists, EffectiveSettings)` 自建实体与预填、字段
  全私有、单一类型化出口 `edited_settings` 完成校验取值；theme 合并与落盘
  编排留在外壳，候选枚举按 connect 收 font 参数的先例由外壳传入）。
- 代价：测试构建额外启用 gpui `test-support`（proptest 等仅进入测试期依赖）；
  外壳对表单的调用从字段直改改为 `form.update(...)` 装箱。

## 验证

- `cargo test connect::tests`：7 项（4 项纯逻辑原有 + 往返/dirty/展开 3 项新增）。
- 全量 `cargo test` 除存量 ConPTY 冒烟环境抖动外全绿。
- 手工冒烟：打开连接弹窗 → 粘贴 `user@host:port` 失焦拆分 → 载入含转发与跳板的
  已保存连接 → 保存并连接；回车提交与 Esc 关闭语义不变。

## 修订（2026-10-27，按 ADR-048）

连接弹窗改为列表优先、编辑与连接分离后，本 ADR 的以下部分不再成立：

- 决策 1 的「持久化字段的编辑监听挂进表单」与决策 6 的「dirty 判定」：主按钮不再
  随改动切换文案，`FormBaseline` / `is_dirty` / 输入框逐字监听整体删除。外壳对表单
  实体的 `observe` 保留，只服务开关、折叠与转发草稿增删的重渲染。
- 决策 4 的「载入后的焦点决策、表单返回焦点目标」：`load_saved` 不再返回焦点句柄，
  进编辑视图一律聚焦主机框（密码框的焦点决策属于「载入即连接」的旧语义）。
- 自持建造、字段全私有、两个类型化出口（`saved_entry` / `connect_target`）、回调经
  枚举分发、副作用留外壳——这些纪律不变，并新增第三个出口 `connect::target_of_saved`
  （已保存条目不经表单直接装配会话目标）。
