# ADR-062：锚定浮层的常驻滚动条

状态：已接受（2026-11-23）

## 背景

字体、Shell、编辑器、WSL 镜像与主题下拉共用 `ui::select::anchored_popover`。
列表超过 `max_h` 时只有 `overflow_y_scroll`，没有位置提示：33 套主题的当前项
常被埋在折叠线以下，用户也不知道还能滚多少。只在主题下拉里做「当前项置顶」
是缓解，不是通用机制——字体列表、镜像列表同样需要位置感。

## 决策

### 1. 结构与归属

- 新增 `ui::scrollbar::Scrollbar`：拖拽状态是 `Rc<Cell<Option<DragState>>>`，
  由浮层所有者（ThemePicker、SettingsForm 的 DropdownState、WslManagerPage）
  持有，跨帧复用。
- `anchored_popover` 的浮层本体就是滚动容器（`max_h` + `overflow_y_scroll`
  + `track_scroll`）；滚动条作为 shell 的 **sibling** 覆盖右缘。
  - 不能把滚动条放进滚动容器：会跟着内容一起滚。
  - 不能套一层 `max_h` 子容器：子容器按完整内容高度撑开，不产生
    `max_offset`，滚动条/滚轮都失效。
- 滚动条层是两个 absolute 子元素：绘制 canvas（右缘 12px 条）与命中 div
  （右缘 10px 条，`occlude`）。都贴最近的 positioned 祖先（shell），
  不依赖零尺寸包裹层。
- 内容区在滚动容器上加 8px 右 gutter，thumb 不压住行文字。

### 2. 几何与交互

- `ScrollHandle::max_offset()` 是**正的可滚距离**，`offset` 向下为负；
  因此 `content = viewport + max_scroll`，`progress = -offset / max_scroll`。
- track/thumb 宽 3px、thumb 最小 24px、内缩 2px；thumb 长度按
  `viewport/content`，位置按 progress。
- 拖动 thumb：`drag_offset = clamp(start_offset - delta_y * max_scroll /
  thumb_travel, -max_scroll, 0)`；点击 track：按指针位置把 thumb 中心
  跳到指针并进入拖拽。
- 拖拽中的全局 move/up 监听在 canvas 的 paint 阶段每帧注册（GPUI 的
  `on_mouse_event` 只对下一帧有效），`set_offset` 后 `window.refresh()`；
  拖拽中 `stop_propagation`，不会顺带选中/删除行。
- 短列表（`max_scroll <= 0`）不画 track/thumb，命中带也不吃点击。

### 3. 主题下拉的顺序与居中

主题下拉保持字母序，不把当前主题移到顶部；打开时把当前主题滚到浮层中部
（ADR-022 修订）。滚动条负责剩余项的位置感，居中负责打开那一刻当前项
可见；两者互补，不是二选一。键盘高亮同样走 `scroll_to_item` 保持可见。

### 4. 测试

- `metrics_for`：内容不超高不画；thumb 顶部对齐 track；progress 改变
  thumb 位置；超长内容 thumb 不小于 24px。
- `drag_offset`：向下拖 offset 更负、两端夹取、零 thumb 行程保持并夹取。

## 后果

- 所有锚定浮层（字体、Shell、编辑器、WSL 镜像、主题）获得常驻、可拖拽
  的滚动条；短列表不显示。
- 滚动条是覆盖层，内容右侧固定让出 8px；行宽略减。
- 滚动条不进入 AX 树（装饰性控件，键盘用户用列表的 ↑↓/Home/End 导航）。
