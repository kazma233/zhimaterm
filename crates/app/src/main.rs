// Windows release 构建隐藏控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! ZhimaTerm 入口：打开主窗口并决定退出时机。
//!
//! 模块分层与依赖方向见 `docs/decisions/007-module-boundaries.md`。速览：
//! - zhimaterm-app（本 crate，workspace 唯一 bin）只有外壳与视图：
//!   - `app`：外壳状态机——会话生命周期、模态互斥、键分发与渲染装配
//!     （按变更轴拆为 sessions / workspace_io / modals / keys / render 子模块）
//!   - `views`：跨业务的共享渲染件——网格绘制、会话画面、SFTP 面板、分屏、
//!     框架部件；探针面板与历史模态随各自的域走
//!   - `settings` / `workspace`：设置表单领域与工作区持久化
//! - 强制的分层边界在 workspace crate：
//!   - `zhimaterm-base`：叶子与基础层——config_paths / font / input / theme / ui / wake
//!   - `zhimaterm-terminal`：终端模型（VT 状态、颜色、转录；零 gpui 引用）
//!   - `zhimaterm-runtime`：会话协议与传输、SFTP、本地终端与会话留痕
//!   - `zhimaterm-ssh`：SSH 连接管理领域（表单、保存连接、钥匙串、转发规则）

mod app;
mod editor;
mod settings;
mod views;
mod workspace;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, point, px, size};
#[cfg(target_os = "macos")]
use gpui::{Menu, MenuItem, SystemMenuType};

use app::ZhimaTermApp;
use zhimaterm_base::input::{Quit, app_key_bindings};

fn main() {
  gpui_platform::application().run(|cx: &mut App| {
    cx.bind_keys(app_key_bindings());
    // 输入组件的编辑键位（选区、剪贴板、撤销重做、多行换行等）：
    // 命中即被输入框消费，终端等其他焦点位置没有处理器时按键回落给 on_key_down
    cx.bind_keys(zhimaterm_base::ui::input::bindings());
    cx.on_action::<Quit>(|_, cx| cx.quit());
    #[cfg(target_os = "macos")]
    set_app_menu(cx);

    let bounds = Bounds::centered(None, size(px(1080.), px(720.)), cx);

    // 工作区（ADR-037）：窗口几何与本地布局。读失败时回退默认窗口并报错，
    // 不覆盖原文件；布局在应用启动后恢复。
    let (workspace, workspace_error) = match workspace::Workspace::load() {
      Ok(workspace) => (workspace, None),
      Err(error) => (None, Some(format!("{error:#}"))),
    };
    let displays: Vec<_> = cx.displays().iter().map(|display| display.bounds()).collect();
    // 几何恢复不做内容区/外框换算：自绘顶栏（ADR-057）后 macOS 走
    // FullSizeContentView，内容区矩形 == 窗口外框矩形，workspace 保存的
    // window_bounds()（外框）回填 window_bounds（按内容区初始化）两侧天然
    // 对称。此前标准标题栏时代的高度补偿已随 ADR-057 移除。
    let window_bounds = match workspace
      .as_ref()
      .and_then(|workspace| workspace.window_bounds(&displays))
    {
      Some(bounds) => bounds,
      None => WindowBounds::Windowed(bounds),
    };
    let main_window = cx
      .open_window(
        WindowOptions {
          window_bounds: Some(window_bounds),
          titlebar: Some(gpui::TitlebarOptions {
            title: Some("ZhimaTerm".into()),
            // 自绘顶栏（ADR-057）：系统标题栏退场，顶栏由 views::title_bar 画
            appears_transparent: true,
            // 红绿灯挪进顶栏高度内（仅 macOS 生效，其余平台忽略该字段）
            traffic_light_position: Some(point(px(12.), px(12.))),
          }),
          // macOS：AppKit 让出标题栏区域的拖拽与双击判定，顶栏按钮才能收到
          // 点击，拖拽由 views::title_bar 的状态机驱动（start_window_move）；
          // Windows 的拖拽走 HTCAPTION 原生命中，无此语义
          #[cfg(target_os = "macos")]
          app_owns_titlebar_drag: true,
          // Linux 请求 CSD：compositor 可协商回 Server 装饰，运行时以
          // window.window_decorations() 为准（顶栏据此决定画不画控制按钮）；
          // macOS/Windows 的装饰由 appears_transparent 承担，该请求被忽略
          #[cfg(any(target_os = "linux", target_os = "freebsd"))]
          window_decorations: Some(gpui::WindowDecorations::Client),
          ..Default::default()
        },
        move |window, cx| cx.new(|cx| ZhimaTermApp::new(window, workspace, workspace_error, cx)),
      )
      .expect("打开 ZhimaTerm 窗口失败");
    let main_window_id = gpui::AnyWindowHandle::from(main_window).window_id();
    cx.on_window_closed(move |cx, _| {
      if !cx.windows().iter().any(|window| window.window_id() == main_window_id) {
        cx.quit();
      }
    })
    .detach();
    cx.activate(true);
  });
}

/// 应用菜单（仅 macOS）。菜单项的键等效提示取自 keymap 绑定（`input::app_key_bindings`），
/// 点击菜单与按 ⌘Q 都以 Quit action 派发到上面注册的处理器。Windows/Linux 没有系统
/// 菜单栏，退出只走 keymap 的按键路径。
#[cfg(target_os = "macos")]
fn set_app_menu(cx: &mut App) {
  cx.set_menus([Menu::new("ZhimaTerm").items([
    MenuItem::os_submenu("服务", SystemMenuType::Services),
    MenuItem::separator(),
    MenuItem::action("退出 ZhimaTerm", Quit),
  ])]);
}
