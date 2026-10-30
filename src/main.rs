// Windows release 构建隐藏控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! ZhimaTerm 入口：打开主窗口并决定退出时机。
//!
//! 模块分层与依赖方向见 `docs/decisions/007-module-boundaries.md`。速览：
//! - `app`：外壳状态机——会话生命周期、模态互斥、键分发与渲染装配
//! - `views`：业务渲染件——网格绘制（`grid`）、会话画面（`terminal_view`）、
//!   SFTP 面板、分屏（`panes`）、框架部件（`shell`）
//! - `connect` / `settings`：连接与设置两个表单领域（含各自的模态渲染）
//! - `terminal` / `session` / `sftp`：终端模型 / 会话协议与传输、SFTP 命令契约
//!   （见 ADR-001/004/005）
//! - `output_log` / `connections`：会话输出日志与保存的连接（见 ADR-017/010）
//! - `input` / `font` / `ui`：输入语义转换 / 字体领域 / 通用组件

mod app;
mod config_paths;
mod connect;
mod connections;
mod cwd;
mod font;
mod forward_form;
mod input;
mod keychain;
mod output_log;
// 跨进程读 PEB 参数块（cwd/argv 采集）只存在于 Windows 生产路径
#[cfg(windows)]
mod peb;
mod probe;
mod pty;
mod recorder;
mod session;
mod settings;
mod sftp;
mod shells;
mod terminal;
#[cfg(test)]
mod tests;
mod theme;
mod ui;
mod views;
// WSL 只在 Windows 生产存在（ADR-021）；解析层的 fixture 测试保留在所有平台的测试构建里
mod workspace;
#[cfg(any(windows, test))]
mod wsl;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
#[cfg(target_os = "macos")]
use gpui::{Menu, MenuItem, SystemMenuType};
#[cfg(target_os = "macos")]
use {
    objc2::MainThreadMarker,
    objc2_app_kit::{NSWindow, NSWindowStyleMask},
    objc2_foundation::{NSPoint, NSRect, NSSize},
};

use app::ZhimaTermApp;
use input::{Quit, app_key_bindings};

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        cx.bind_keys(app_key_bindings());
        // 输入组件的编辑键位（选区、剪贴板、撤销重做、多行换行等）：
        // 命中即被输入框消费，终端等其他焦点位置没有处理器时按键回落给 on_key_down
        cx.bind_keys(ui::input::bindings());
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
        let displays: Vec<_> = cx
            .displays()
            .iter()
            .map(|display| display.bounds())
            .collect();
        let window_bounds = match workspace
            .as_ref()
            .and_then(|workspace| workspace.window_bounds(&displays))
        {
            Some(bounds) => restore_window_bounds(bounds, titlebar_height()),
            None => WindowBounds::Windowed(bounds),
        };
        let main_window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(window_bounds),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("ZhimaTerm".into()),
                        appears_transparent: false,
                        traffic_light_position: None,
                    }),
                    ..Default::default()
                },
                move |window, cx| {
                    cx.new(|cx| ZhimaTermApp::new(window, workspace, workspace_error, cx))
                },
            )
            .expect("打开 ZhimaTerm 窗口失败");
        let main_window_id = gpui::AnyWindowHandle::from(main_window).window_id();
        cx.on_window_closed(move |cx, _| {
            if !cx
                .windows()
                .iter()
                .any(|window| window.window_id() == main_window_id)
            {
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

/// 恢复窗口几何：gpui 0.3.10 的 macOS open_window 把 WindowOptions.window_bounds
/// 当内容区矩形（initWithContentRect），而保存的 window_bounds() 是含标题栏的
/// 窗口外框；不换算窗口每次重启膨胀一个标题栏高。gpui 修正该语义后移除这里。
/// 仅动高度：标准标题栏只占竖向空间，x/y/宽度的保存恢复已实测往返稳定。
fn restore_window_bounds(bounds: WindowBounds, titlebar_height: f32) -> WindowBounds {
    match bounds {
        WindowBounds::Windowed(mut bounds) => {
            bounds.size.height -= px(titlebar_height);
            WindowBounds::Windowed(bounds)
        }
        // workspace 保存时已把最大化/全屏压平成窗口态，此分支不会被走到
        other => other,
    }
}

/// 标准窗口的标题栏高度。Windows/Linux 的 gpui 保存恢复两端对称，无需补偿。
/// AppKit 契约：NSWindow 类方法只允许主线程调用（绑定用 MainThreadMarker 强制），
/// 因此不做离线单测——换算逻辑由本模块其余测试守住，这里的调用点在
/// application().run 的主线程上。
#[cfg(target_os = "macos")]
fn titlebar_height() -> f32 {
    // mask 对应 main() 里 WindowOptions 组装出的样式（标准标题栏、可缩放、
    // 可最小化）；调整 TitlebarOptions/is_resizable/is_minimizable 需同步这里
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let empty = NSRect {
        origin: NSPoint::new(0., 0.),
        size: NSSize::new(0., 0.),
    };
    // AppKit 类方法只能在主线程调；这里的调用点都在 application().run 的主线程上
    let mtm = MainThreadMarker::new().expect("titlebar_height 必须在主线程调用");
    let frame = NSWindow::frameRectForContentRect_styleMask(empty, style, mtm);
    frame.size.height as f32
}

#[cfg(not(target_os = "macos"))]
fn titlebar_height() -> f32 {
    0.
}

#[cfg(test)]
mod window_restore_tests {
    use super::restore_window_bounds;
    use gpui::{Bounds, WindowBounds, point, px, size};

    #[test]
    fn restore_only_shrinks_height() {
        let bounds = WindowBounds::Windowed(Bounds::new(
            point(px(337.), px(30.)),
            size(px(1264.), px(957.)),
        ));
        let WindowBounds::Windowed(restored) = restore_window_bounds(bounds, 32.) else {
            panic!("应保持窗口态");
        };
        assert_eq!(
            (f32::from(restored.origin.x), f32::from(restored.origin.y)),
            (337., 30.)
        );
        assert_eq!(
            (
                f32::from(restored.size.width),
                f32::from(restored.size.height)
            ),
            (1264., 925.)
        );
    }

    #[test]
    fn restore_is_identity_without_titlebar() {
        let bounds =
            WindowBounds::Windowed(Bounds::new(point(px(1.), px(2.)), size(px(300.), px(400.))));
        assert_eq!(restore_window_bounds(bounds, 0.), bounds);
    }
}
