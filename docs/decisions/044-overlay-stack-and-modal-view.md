# ADR-044：统一浮层栈与 ModalView 接入规范

状态：已采纳（2026-10-22）

## 背景

浮层状态有三套平行实现：`app.modal: Option<Modal>`（互斥单点，五个变体）、
`confirm_close`（独立字段）、`theme_picker`（独立状态）。z 序靠 render 里 child
声明顺序硬编码，键路由优先级靠 `overlay_route` 的 if 硬编码。新增一个模态要改
app.rs 三处 match（`Modal` enum、`overlay_key_route`、render 分支）。深层视图
（SFTP 面板、探针面板）不持有 app 句柄，不存在呼出 app 级浮层的通道。

需求：任意界面可呼出自定义模态、置顶于所有 UI、传递按钮事件与用户输入
（对齐前端 dialog service 的使用体感）。

## 决策

### 1. OverlayHost 栈是浮层的唯一归属

`Vec<Layer>` 栈替代 `Option<Modal>` 与 `confirm_close` 两个状态。栈序即 z 序
（render 按栈序挂载在根部末尾），键路由只裁决栈顶，焦点每层独立借还。

```rust
struct Layer {
    species: Species,               // Modal | Dialog：叠放判定
    view: Box<dyn ModalView>,
    scope: FocusHandle,             // ADR-043 作用域，每层独立
    focus_owner: Option<FocusHandle>, // 入栈记录，出栈归还
}
```

叠放规则：**同物种互斥替换**（新开即替换旧层，旧层 owner 链丢弃，等价现状
`open_modal`），**跨物种叠放**（Dialog 可盖 Modal，后入栈者接管键盘与焦点）。
无 layer id，重复 open 即替换。

入栈成员：存量五个模态 + confirm_close。theme_picker 不入栈（锚定浮层，保持
「任何层入栈即收起」互斥）；SFTP/probe 视图内浮层不入栈（遮罩只盖面板区域是
产品语义，ADR-043 已统一其焦点物种）。

### 2. ModalView 是接入契约（slot 式区域规范）

```rust
pub trait ModalView {
    fn species(&self) -> Species;
    fn tone(&self) -> Tone;                       // Normal | Warn | Danger，卡片皮肤
    fn title(&self) -> Option<String>;
    fn content(&self, window: &Window, cx: &App) -> AnyElement;
    fn footer(&self, cx: &App) -> Option<AnyElement>;
    fn actions(&self, cx: &App) -> AnyElement;
    fn key_action(&self, key: &Keystroke) -> KeyHandling; // 默认 Esc = Close
    fn on_dismiss(&self) -> KeyHandling { KeyHandling::Close } // 点遮罩语义
    fn focus_ring(&self, cx: &App) -> Vec<FocusHandle> { vec![] }
}

pub enum KeyHandling {
    None,                                          // 不认，走正常路径
    Close,                                         // 关当前层
    Action(Rc<dyn Fn(&mut Window, &mut App)>),     // 业务语义动作
}
```

壳统一组装 `Card`（四区 header/content/footer/actions 是类型契约）+ `panel::modal`
遮罩/居中/焦点壳（ADR-043）。业务模态自持数据（表单按 ADR-026 实体化）与回调
集合（ADR-007 单向依赖），`KeyHandling::Action` 携带闭包，键盘裁决保持纯函数可测。
设置表单的「遮罩点击直接关、Esc 先收下拉」差异由 `on_dismiss` 覆写表达。

**豁免记录**：栈层是 `Box<dyn ModalView>` 开放接入（业务实现 trait 即可呼出，
壳零改动），放弃 `Modal` enum 穷举「漏写编译失败」保障；防漏价值由 trait 默认
实现承接（漏写 `key_action` 落到默认 Esc 关闭，行为安全）。`OverlayRoute` /
`OverlayKeyAction` / `overlay_key_route` 随之删除，业务键语义回到各自模态。

### 3. 呼出通道：Global 放实体句柄，状态住 app

`ModalHost` 状态是 `ZhimaTermApp` 的字段（与 tabs 同级的唯一所有者，open 内可
协调 theme_picker 互斥），`Global` 只注册 `Entity<ZhimaTermApp>` 句柄。任意视图
的事件回调里：

```rust
ui::overlay::open(box view, window, cx);   // 内部 update app → host.open(...)
```

写入单点收口在 `OverlayHost::open / close_top / close_all`，每个方法一次完成
push+focus / pop+归还，原子边界成立。

### 4. 通用 Dialog（ui/dialog.rs，ModalView 之上封装）

```rust
Dialog::confirm(title, message).danger(true).confirm_label("删除")
Dialog::prompt(title, message, placeholder)
// host.open_dialog(dialog, |answer: DialogAnswer, window, cx| ...)
// answer = Confirmed | ConfirmedWithInput(String) | Cancelled
```

键默认 Enter=确认、Esc=取消；点遮罩恒为「取消」（取消总是安全，对齐现状
confirm_close）。species = Dialog（可叠在 Modal 上，Dialog 之间互斥替换）。

### 5. 存量迁移（一次到位）

Connect/Settings/Forward/Shortcuts/Wsl 各自成为领域模块里的 `XxxModal` 实现
`ModalView`；confirm_close 迁为 `ConfirmCloseModal`（Tone::Warn 保留现有警示
样式，`Card` 增加 tone 皮肤参数）。DeadSession 键路由保留在 app
`handle_key_down`（它不是浮层，条件从 `overlay_route` 改为栈空）。

## 后果

- 新增业务模态 = 领域模块写一个 struct 实现 `ModalView`（按 ADR-027 判据选
  归属），任意视图一行呼出，不改壳。
- 简单确认/输入场景直接用通用 Dialog，不写新类型。
- 单测：OverlayHost 状态机（替换/叠放/归还链/close_all）、Dialog answer
  映射为纯逻辑单测；渲染编排不设单测，手工验收（ADR-043 口径）。
- 手工验收：存量五模态 + confirm_close 行为等价（焦点无高亮、Tab 环、Enter
  双语义、下拉先收、遮罩语义）；新能力在 SFTP 面板呼出 confirm/prompt 验证
  回调、输入回传与焦点归还。
- `Box<dyn ModalView>` 与「穷举编译保障」价值观的冲突已按当前需求裁决并记录
  于决策 2；若未来出现需要集中路由的物种再议。

## 实施记录（2026-10-22 落地）

- trait 落地时补了三处签名细节：`key_action` 带 `cx`（连接表单的回车语义要
  读实体 dirty）；覆写的回落分支调 `overlay::default_key_action` 自由函数
  ——不能写 `ModalView::key_action(self, ..)`，完全限定调用同样解析回覆写
  实现，会无限递归；`as_any` / `as_any_mut` 供外壳编排动作下钻栈顶模态
  数据（`connect_form()` 等），不实现则外壳拿不到、安全降级 None。
- 模态需要跨归属数据（已保存连接、设置读盘错误、确认框文案）时经
  `app::saved_view` / `app::settings_error` 全局只读，不在模态里复制状态。
- 设置表单的下拉浮层经 trait 的 `floaters` 挂在遮罩层内、卡片上层，与原先
  直接 append 到 modal Div 的层级一致。
- 连接表单是大分区模态：覆写 `card` 定制尺寸（七成宽/八成高），`Card`
  增加通用能力 `tone`（Warn/Danger 皮肤）与 `full_content`（内容区无内边距、
  纵向伸展），不是业务特例。
- 三处与旧实现的可感差异，均为决策内的归一而非回归：
  1. 确认框卡片归一到 Card 规范（宽 420 替代原 380、圆角 10 替代 8、
     分区内边距），警示配色不变；
  2. 连接表单底栏内边距 10→12（Card 动作区规范）；
  3. 确认框盖在表单上时 Tab 走确认框的作用域（原实现会穿到被盖住的表单
     输入框，属于焦点逃逸，按「只裁决栈顶」修正）。
- 通用 Dialog（`ui/dialog`）按 `ui/input` 的先例挂 `#[allow(dead_code)]`：
  能力先就位，接入点等首个真实需求。
- Windows 侧（WslModal 与其键路由）未在 Windows 上编译验证（本机为 macOS），
  与仓库 Windows 验证现状一致，标记 unverified。

## 实施修正（2026-10-22：首跑 panic「cannot read ZhimaTermApp while it is
already being updated」）

模态渲染与键盘裁决都发生在应用实体的 update（render / key_down listener）里，
两条硬约束随之落地：

1. **render 路径不得经全局句柄读应用实体**。已保存连接改为独立实体
   `Entity<SavedSources>`（外壳唯一所有者，连接模态持同一实体只读渲染——
   忘掉/保存后列表即时刷新，数据单源）；设置读盘错误与确认框文案改为打开
   时快照（模态期间这两种数据不可能变化，互斥收起了唯一的写入入口）。
   `saved_view` / `settings_error` 全局读取函数删除。
2. **键裁决产生的 Action 推迟到下一帧执行**（`window.on_next_frame`）。
   Action 闭包多经 listener 包装（回调内对本实体再 update），而键盘路径已在
   本实体的 key_down listener（update）里：同步执行会嵌套 update 同一实体；
   `cx.defer_in` 也不行——它的回调跑在该实体的 update 内，闭包里再 update
   同样算嵌套。`on_next_frame` 的回调在 run loop 的帧绘制里执行，不在任何
   实体的 update 内，闭包里的 update 是合法单层；对用户动作延迟一帧不可感。
   键盘与事件回调两条路径统一走这一处派发。


## 追加（2026-10-22 晚）：操作记录解耦与日志运行时开关

SFTP 操作历史从面板实体解耦，成为一份「操作记录」全屏模态，入口在底栏
右下角的「历史」按钮（SSH 会话即显示，不依赖 SFTP 面板开合，甚至不依赖
SFTP 子系统）。（2026-10-23 修订：原拟与会话输出日志（ADR-017）汇成同一份
模态，后按下条拆成两个入口——会话日志的开关与打开在独立的「记录」
模态。）

- 历史数据是独立实体 `Entity<SessionHistory>`，随格销毁
  （「关闭会话即清空」语义不变）；SFTP 面板在任务结束时写入（唯一写入方），
  模态与面板传输区各自只读。TaskItem 家族与历史渲染移入 `views/history.rs`。
  （2026-10-22 按 ADR-045 修订：实体的归属从「挂在终端视图字段上」收进
  全局记录门面的注册表，SFTP 面板只持格 id、写入走 `record_operation`，
  「随格销毁」由 `TerminalView::drop` 注销实现。）
- 面板内的历史弹层与「操作历史 N」入口删除；入口统一收到底栏。
  （2026-10-23 追记：传输区最近几行的预览与运行中任务区也整体删除——新操
  作由底栏「历史」按钮的高亮提示（`SessionHistory` 的未读标记，落历史置
  位、打开模态清除），进行中的传输不再有面板内进度视图。）
- 会话日志的写入开关与打开不进操作记录模态，是独立入口（2026-10-23
  修订原「放进操作记录模态」的方案）：底栏「历史」旁的「记录」按钮开
  「记录」模态（记录中/未开启状态、日志文件名、「暂停记录/记录到文件」
  切换、「打开」外部查看），操作记录模态只含操作记录。开关随时切换、
  不写进任何设置或连接存储。实现：开关经记录门面（ADR-045）切注册条目
  里 `Arc<Mutex<Option<SessionLog>>>` 的 Option（worker 线程与视图共享
  同一把锁，切换即刻生效）；关 = 停写并落「记录暂停」标记，
  开 = 按原文件追加重开（文件按格分配不变）并落「记录开启」标记。
- 开关的已知边界：转录按「滚出屏幕进回滚」计（ADR-017 修订），暂停时
  还在屏幕上、恢复后才滚出的内容会随恢复后的转录落盘——行没有产生时间
  的元数据，按游标无法区分；暂停期间已滚出的行由 worker 的恢复边沿排掉
  （有 e2e 测试守住：`runtime_switch_pauses_and_resumes_without_backfill`）。

## 修订（2026-10-27）：浮层栈非空时会话级动作不派发

落地侧栏重命名（ADR-047）时发现：⌘T（NewSession）与 ⌘W（CloseSession）是
`Scope::App` 的无上下文绑定，模态开着时照样命中——gpui 先沿焦点路径匹配
键位绑定、命中的 action 被根节点 `on_action` 消费后直接返回，裸键根本不会
派发到 `handle_key_down`，决策 1 的「栈顶浮层在场时按键全归它」只覆盖了
裸键路径。后果是在模态底下开会话（`activate` 把焦点抢到新格，焦点逃出模态，
正是 ADR-043 要防的）或关会话（删标签；需确认时弹出的关闭确认是 Dialog
物种，会顶掉正在填的重命名对话框）。

修法：应用根的两个 `on_action` 处理器在 `overlay` 非空时直接返回。不改键位
层——`!Modal` 这类谓词在上下文栈为空时不匹配（`eval_inner` 对空栈返回
false），要走声明式方案得再给根节点挂常驻上下文、并给 ⌘Q 开例外，改动面
大于收益；⌘Q 在 `main.rs` 以 App 级 `on_action` 注册，不经根节点，仍随时可退。
⌘D/⌘⇧D/⌘F 是 `Terminal` 上下文绑定，模态开着时网格不持焦、本就不命中。
视图内弹层（SFTP/探针的 `panel::modal`）不在浮层栈里，不受本修订影响。
快捷键参考的应用级分组标题随之注明「弹窗开着时不开/关会话」。

## 追记（2026-10-30 补记）：面板内进行中任务区回补

上文 2026-10-23 追记的「运行中任务区整体删除」当天稍后被 066e6ec 部分回退：
SFTP 面板重新加入**进行中任务区**——每条任务两行，任务描述与「取消」在上、
进度文本在下（树传输经进度事件带出正在处理的文件名）；取消链路此前已实现但
无 UI 入口。已结束操作的预览与「操作历史 N」入口不回补：历史入口仍在底栏
「历史」按钮，未读高亮语义不变。
