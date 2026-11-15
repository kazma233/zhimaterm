//! WSL 管理（仅 Windows，ADR-021 的延伸）：底部 WSL 入口呼出的管理模态。
//! 分发版列表（磁盘占用/运行状态/安装路径/WSL 版本展示）+ 行操作（开会话、
//! 设默认、终止、设置、删除）+ 创建（新增 WSL，候选选择在弹出的模态里）
//! + 维护（全部关闭、更新 WSL、刷新）。不提供导入与 WSL1↔2 转换——版本
//! 只作展示。
//!
//! 页面实体自持全部状态与 wsl.exe 编排（阻塞调用进后台线程，完成后回写
//! 实体重绘），外壳只注入「开会话」回调并经浮层栈呼出 [`WslManagerModal`]。
//! 危险操作（删除）经全局 Dialog 叠确认；Esc/点遮罩/右下「关闭」都关模态。

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, AppContext, Bounds, Context, Entity, FocusHandle, Keystroke, Pixels, Window, div, px};

use zhimaterm_base::theme::{self, UiTokens};
use zhimaterm_base::ui::dialog::{self, Dialog, DialogAnswer};
use zhimaterm_base::ui::human_bytes;
use zhimaterm_base::ui::input::{TextInput, field as input_field, field_with_hint as input_field_with_hint};
use zhimaterm_base::ui::overlay::{KeyHandling, ModalView, Species, bare_key};
use zhimaterm_base::ui::panel;
use zhimaterm_base::ui::select::{plain_select_popover, plain_select_row, select_backdrop};
use zhimaterm_base::ui::ui_rems;

/// 页面要执行的语义命令：wsl.exe 子命令的薄包装。危险/耗时项由
/// [`WslManagerPage::run`] 先叠确认对话框再执行。
#[derive(Clone, Debug)]
enum Command {
  /// 新增分发版（`wsl --install`，带用户自定义的安装设置；自定义用户名
  /// 在装完后经 `--manage --set-default-user` 落地）
  Install {
    distro: String,
    /// `--location`；None = 系统默认安装位置
    location: Option<String>,
    /// `--name`；None = 与分发版同名
    name: Option<String>,
    /// 装完创建并设为默认的用户；None = 分发版默认（首次启动引导）
    user: Option<String>,
    /// `--no-launch`：装完不自动进首次设置，回列表由用户自己开
    no_launch: bool,
    /// `--web-download`：走网络源而非 Microsoft Store
    web_download: bool,
    /// `--fixed-vhd`：固定大小磁盘
    fixed_vhd: bool,
    /// `--vhd-size`；None = 默认大小（如 `256GB`）
    vhd_size: Option<String>,
  },
  /// 设为默认分发版（`wsl --set-default`）
  SetDefault(String),
  /// 终止分发版（`wsl --terminate`）
  Terminate(String),
  /// 删除（`wsl --unregister`），必须先危险确认
  Unregister(String),
  /// 稀疏 VHD 开关（`wsl --manage --set-sparse`，开了自动回收磁盘空间）
  SetSparse { name: String, sparse: bool },
  /// 移动分发版到新位置（`wsl --manage --move`，耗时，需确认）
  Move { name: String, location: String },
  /// 调整分发版磁盘大小（`wsl --manage --resize`，如 `512GB`）
  Resize { name: String, size: String },
  /// 全部关闭（`wsl --shutdown`）
  Shutdown,
  /// 强制关闭（`wsl --shutdown --force`，可能数据丢失，必须危险确认）
  ShutdownForce,
  /// 更新 WSL 本体（`wsl --update`）
  Update,
}

impl Command {
  /// 状态行里的动作名（「正在安装 Ubuntu…」）。
  fn label(&self) -> String {
    match self {
      Command::Install { distro, .. } => format!("安装 {distro}"),
      Command::SetDefault(name) => format!("设置 {name} 为默认"),
      Command::Terminate(name) => format!("终止 {name}"),
      Command::Unregister(name) => format!("删除 {name}"),
      Command::SetSparse { name, sparse } => {
        format!("设置 {name} 磁盘为{}稀疏", if *sparse { "" } else { "非" })
      }
      Command::Move { name, .. } => format!("移动 {name}（耗时与目标盘速度相关）"),
      Command::Resize { name, size } => format!("调整 {name} 磁盘为 {size}"),
      Command::Shutdown => "关闭全部 Distribution".into(),
      Command::ShutdownForce => "强制关闭全部 Distribution".into(),
      Command::Update => "更新 WSL".into(),
    }
  }

  /// 成功时 UI 是否已经能看出区别：装完列表多一行、设默认 tag/按钮移动、
  /// 终止/删除/全部关闭后运行态变化——这些不用再弹「完成」；稀疏/移动/
  /// 调整大小/更新在界面上没有可观察的变化（移动还是耗时操作），保留提醒。
  fn success_is_visible(&self) -> bool {
    matches!(
      self,
      Command::Install { .. }
        | Command::SetDefault(_)
        | Command::Terminate(_)
        | Command::Unregister(_)
        | Command::Shutdown
        | Command::ShutdownForce
    )
  }
}

/// 在线候选（`wsl --list --online`）的加载态。不在打开页面时自动拉取——
/// 点「新增 WSL」弹出模态时才拉，加载/失败/候选都在模态里呈现。
enum Online {
  NotLoaded,
  Loading,
  Ready(Vec<String>),
  Failed(String),
}

/// 安装设置的可切换档位。
#[derive(Clone, Copy)]
enum InstallFlag {
  NoLaunch,
  WebDownload,
  FixedVhd,
}

/// 分发版设置会话的装载态：NotRunning（未运行，等用户手动启动）/ Loading
/// （读 /etc/wsl.conf 中）/ Failed（读不到）/ Ready（可编辑）。
enum SettingsState {
  NotRunning { distro: String },
  Loading { distro: String },
  Failed { distro: String, message: String },
  Ready(Box<ConfSession>),
}

/// Ready 态的可编辑会话：`conf` 是草稿（开关翻转直接改它），文本项走输入
/// 框实体；`original` 是读取到的原文（保存时合并基底，保留用户的自定义段）。
struct ConfSession {
  distro: String,
  original: String,
  conf: zhimaterm_local::wsl::WslConfSettings,
  /// [user] default（留空 = 移除键恢复默认）
  user: Entity<TextInput>,
  /// [network] hostname（留空 = Windows 计算机名）
  hostname: Entity<TextInput>,
  /// [boot] command（留空 = 移除键）
  boot_command: Entity<TextInput>,
  /// [boot] initTimeout：等待 systemd 初始化的毫秒数（留空 = 移除键）
  boot_init_timeout: Entity<TextInput>,
  /// [automount] root（留空 = /mnt/）
  automount_root: Entity<TextInput>,
  /// [automount] options（留空 = 移除键）
  automount_options: Entity<TextInput>,
  /// [automount] cgroups：v1 / v2（留空 = 移除键）
  automount_cgroups: Entity<TextInput>,
  /// 管理操作：目标磁盘大小
  resize: Entity<TextInput>,
  /// 管理操作：移动目标位置
  move_to: Entity<TextInput>,
}

/// 设置模态里的布尔开关键（toggle 翻转落点）。
#[derive(Clone, Copy)]
enum ConfBool {
  BootSystemd,
  BootProtectBinfmt,
  NetworkGenerateHosts,
  NetworkGenerateResolvConf,
  InteropEnabled,
  InteropAppendWindowsPath,
  GpuEnabled,
  GpuAppendLibPath,
  TimeUseWindowsTimezone,
  AutomountEnabled,
  AutomountMountFsTab,
  AutomountLdconfig,
}

impl ConfBool {
  /// 当前显示值：草稿已设置用实际值，未设置用 WSL 默认值。
  fn display(self, conf: &zhimaterm_local::wsl::WslConfSettings) -> bool {
    let value = match self {
      ConfBool::BootSystemd => conf.boot_systemd,
      ConfBool::BootProtectBinfmt => conf.boot_protect_binfmt,
      ConfBool::NetworkGenerateHosts => conf.network_generate_hosts,
      ConfBool::NetworkGenerateResolvConf => conf.network_generate_resolv_conf,
      ConfBool::InteropEnabled => conf.interop_enabled,
      ConfBool::InteropAppendWindowsPath => conf.interop_append_windows_path,
      ConfBool::GpuEnabled => conf.gpu_enabled,
      ConfBool::GpuAppendLibPath => conf.gpu_append_lib_path,
      ConfBool::TimeUseWindowsTimezone => conf.time_use_windows_timezone,
      ConfBool::AutomountEnabled => conf.automount_enabled,
      ConfBool::AutomountMountFsTab => conf.automount_mount_fs_tab,
      ConfBool::AutomountLdconfig => conf.automount_ldconfig,
    };
    value.unwrap_or_else(|| self.default_value())
  }

  /// 该键在草稿里是否显式设置过（决定「未设置」状态行）。
  fn is_set(self, conf: &zhimaterm_local::wsl::WslConfSettings) -> bool {
    match self {
      ConfBool::BootSystemd => conf.boot_systemd.is_some(),
      ConfBool::BootProtectBinfmt => conf.boot_protect_binfmt.is_some(),
      ConfBool::NetworkGenerateHosts => conf.network_generate_hosts.is_some(),
      ConfBool::NetworkGenerateResolvConf => conf.network_generate_resolv_conf.is_some(),
      ConfBool::InteropEnabled => conf.interop_enabled.is_some(),
      ConfBool::InteropAppendWindowsPath => conf.interop_append_windows_path.is_some(),
      ConfBool::GpuEnabled => conf.gpu_enabled.is_some(),
      ConfBool::GpuAppendLibPath => conf.gpu_append_lib_path.is_some(),
      ConfBool::TimeUseWindowsTimezone => conf.time_use_windows_timezone.is_some(),
      ConfBool::AutomountEnabled => conf.automount_enabled.is_some(),
      ConfBool::AutomountMountFsTab => conf.automount_mount_fs_tab.is_some(),
      ConfBool::AutomountLdconfig => conf.automount_ldconfig.is_some(),
    }
  }

  /// WSL 官方默认值（learn.microsoft.com/windows/wsl/wsl-config）。
  fn default_value(self) -> bool {
    match self {
      ConfBool::BootSystemd => false,
      ConfBool::BootProtectBinfmt
      | ConfBool::NetworkGenerateHosts
      | ConfBool::NetworkGenerateResolvConf
      | ConfBool::InteropEnabled
      | ConfBool::InteropAppendWindowsPath
      | ConfBool::GpuEnabled
      | ConfBool::GpuAppendLibPath
      | ConfBool::TimeUseWindowsTimezone
      | ConfBool::AutomountEnabled
      | ConfBool::AutomountMountFsTab
      | ConfBool::AutomountLdconfig => true,
    }
  }

  /// 翻转草稿里的值（开关点击落点：未设置时展示的是默认值，点一下写入
  /// 与之相反的显式值；显式后每次点击翻转。写入后保存时才落键）。
  fn toggle(self, conf: &mut zhimaterm_local::wsl::WslConfSettings) {
    let value = !self.display(conf);
    match self {
      ConfBool::BootSystemd => conf.boot_systemd = Some(value),
      ConfBool::BootProtectBinfmt => conf.boot_protect_binfmt = Some(value),
      ConfBool::NetworkGenerateHosts => conf.network_generate_hosts = Some(value),
      ConfBool::NetworkGenerateResolvConf => conf.network_generate_resolv_conf = Some(value),
      ConfBool::InteropEnabled => conf.interop_enabled = Some(value),
      ConfBool::InteropAppendWindowsPath => conf.interop_append_windows_path = Some(value),
      ConfBool::GpuEnabled => conf.gpu_enabled = Some(value),
      ConfBool::GpuAppendLibPath => conf.gpu_append_lib_path = Some(value),
      ConfBool::TimeUseWindowsTimezone => conf.time_use_windows_timezone = Some(value),
      ConfBool::AutomountEnabled => conf.automount_enabled = Some(value),
      ConfBool::AutomountMountFsTab => conf.automount_mount_fs_tab = Some(value),
      ConfBool::AutomountLdconfig => conf.automount_ldconfig = Some(value),
    }
  }
}

/// 行操作按钮的点击动作：目标在装配时绑死（命令/分发版名）。
type RowAction = Rc<dyn Fn(&mut Window, &mut App)>;

/// 点分发版主体后的开会话动作：外壳注入。
pub type PickAction = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// 命令执行动作（行按钮装配共用一份，命令在每处绑死）。
type RunAction = Rc<dyn Fn(Command, &mut Window, &mut App)>;

/// 选择框 bounds 槽位（select 组件契约同款）：渲染帧写入，浮层按它定位。
type BoundsSlot = Rc<Cell<Bounds<Pixels>>>;

/// WSL 管理页面。`busy` 是运行中的命令标签（期间禁用全部操作入口）；命令
/// 结果（成功/失败）完成后以提示弹窗展示，不在页面留状态。
pub struct WslManagerPage {
  distros: Vec<zhimaterm_local::wsl::WslDistro>,
  busy: Option<String>,
  online: Online,
  /// 在线安装：选中的候选下标（None = 未选择）
  install_selected: Option<usize>,
  /// 镜像下拉是否展开
  install_open: bool,
  /// 镜像下拉选择框的 bounds 槽位（展开的浮层按它定位）
  install_bounds: BoundsSlot,
  /// 镜像下拉键盘高亮（None = 无高亮）
  install_highlight: Option<usize>,
  /// 镜像下拉候选滚动
  install_scroll: gpui::ScrollHandle,
  /// 新增模态内的校验错误（用户名非法这类）：页面头部状态行在模态遮罩后
  /// 看不见，错误只在模态说明区显示
  install_notice: Option<String>,
  /// [user] default 之外的安装自定义设置（常驻输入框）
  install_location: Entity<TextInput>,
  install_name: Entity<TextInput>,
  install_user: Entity<TextInput>,
  install_vhd_size: Entity<TextInput>,
  /// `--no-launch`，默认开
  install_no_launch: bool,
  /// `--web-download`，默认关
  install_web_download: bool,
  /// `--fixed-vhd`，默认关
  install_fixed_vhd: bool,
  /// 分发版设置模态会话：Some 时模态显示设置内容（None = 正在读取）
  distro_settings: Option<SettingsState>,
  /// WSL 本体版本（只读展示，打开页面时后台拉）
  wsl_version: Option<String>,
  /// 页面焦点根：进入页面时焦点落这里，Esc 由页面自己裁决
  focus: FocusHandle,
  /// 输入框用终端等宽字体（与连接/设置表单同一口径）
  font: gpui::Font,
  /// 点分发版主体后的开会话动作：外壳注入
  on_pick: PickAction,
}

impl WslManagerPage {
  pub fn new(font: gpui::Font, on_pick: PickAction, cx: &mut Context<Self>) -> Self {
    let form_input =
      |placeholder: &str, cx: &mut Context<Self>| cx.new(|cx| TextInput::new(cx, placeholder, false, font.clone()));
    let mut page = Self {
      distros: Vec::new(),
      busy: None,
      online: Online::NotLoaded,
      install_selected: None,
      install_open: false,
      install_bounds: Rc::new(Cell::new(Bounds::default())),
      install_highlight: None,
      install_scroll: gpui::ScrollHandle::new(),
      install_notice: None,
      install_location: form_input("默认（用户目录）", cx),
      install_name: form_input("与 Distribution 同名", cx),
      install_user: form_input("Distribution 默认（首次启动引导创建）", cx),
      install_vhd_size: form_input("默认大小", cx),
      install_no_launch: true,
      install_web_download: false,
      install_fixed_vhd: false,
      distro_settings: None,

      wsl_version: None,
      focus: cx.focus_handle(),
      font,
      on_pick,
    };
    // 首屏数据自起后台加载（注册表 + 磁盘占用 + WSL 版本）：入口先渲染
    // 空态不卡点击
    page.refresh(cx);
    page.load_version(cx);
    page
  }

  /// 是否有命令在跑：期间所有操作按钮 no-op。
  fn busy_idling(&self) -> bool {
    self.busy.is_some()
  }

  // -----------------------------------------------------------------------
  // 编排：命令执行与数据加载。阻塞的 wsl.exe / 注册表调用都进后台线程，
  // 完成回写实体；页面可能已被关掉，update 失败静默丢弃。
  // -----------------------------------------------------------------------

  /// 重查注册表刷新分发版列表。
  fn refresh(&mut self, cx: &mut Context<Self>) {
    if self.busy_idling() {
      return;
    }
    cx.spawn(async move |this, cx| {
      let distros = cx
        .background_executor()
        .spawn(async move { zhimaterm_local::wsl::list_distros() })
        .await;
      let _ = this.update(cx, |page, cx| {
        page.distros = distros;
        cx.notify();
      });
    })
    .detach();
  }

  /// 执行命令：进忙碌态，完成后刷新列表并以提示弹窗展示结果。
  fn execute(&mut self, command: Command, window: gpui::AnyWindowHandle, cx: &mut Context<Self>) {
    if self.busy_idling() {
      return;
    }
    let label = command.label();
    // command 随后台任务 move 走，成功是否可见的判定先记下
    let success_is_visible = command.success_is_visible();
    self.busy = Some(label.clone());
    cx.notify();
    cx.spawn(async move |this, cx| {
      let (outcome, distros) = cx
        .background_executor()
        .spawn(async move {
          let outcome = match &command {
            Command::Install {
              distro,
              location,
              name,
              user,
              no_launch,
              web_download,
              fixed_vhd,
              vhd_size,
            } => {
              let installed = zhimaterm_local::wsl::install(
                distro,
                location.as_deref(),
                name.as_deref(),
                *no_launch,
                *web_download,
                *fixed_vhd,
                vhd_size.as_deref(),
              )
              .map(|_| ());
              if installed.is_err() {
                installed
              } else {
                // 自定义用户名在装完后落地（创建用户要短暂启动分发版）；
                // 「装完不自动启动」承诺的是装完的稳态，随后终止恢复
                let settled = match user.as_deref() {
                  Some(user) => zhimaterm_local::wsl::install_set_default_user(distro, user),
                  None => Ok(()),
                };
                match settled {
                  Err(message) => Err(message),
                  Ok(()) if *no_launch => zhimaterm_local::wsl::terminate(distro)
                    .map(|_| ())
                    .map_err(|message| format!("设置用户后终止 Distribution 失败：{message}")),
                  Ok(()) => Ok(()),
                }
              }
            }
            Command::SetDefault(name) => zhimaterm_local::wsl::set_default(name).map(|_| ()),
            Command::Terminate(name) => zhimaterm_local::wsl::terminate(name).map(|_| ()),
            Command::Unregister(name) => zhimaterm_local::wsl::unregister(name).map(|_| ()),
            Command::SetSparse { name, sparse } => zhimaterm_local::wsl::manage_set_sparse(name, *sparse).map(|_| ()),
            Command::Move { name, location } => zhimaterm_local::wsl::manage_move(name, location).map(|_| ()),
            Command::Resize { name, size } => zhimaterm_local::wsl::manage_resize(name, size).map(|_| ()),
            Command::Shutdown => zhimaterm_local::wsl::shutdown(false).map(|_| ()),
            Command::ShutdownForce => zhimaterm_local::wsl::shutdown(true).map(|_| ()),
            Command::Update => zhimaterm_local::wsl::update().map(|_| ()),
          };
          (outcome, zhimaterm_local::wsl::list_distros())
        })
        .await;
      let _ = this.update(cx, |page, cx| {
        page.busy = None;
        page.distros = distros;
        // 命令可能删除了正在编辑设置的分发版：编辑态一并收掉
        page.distro_settings = None;
        cx.notify();
      });
      // 结果以提示弹窗展示（失败红字）；UI 已能体现的成功不再弹；窗口可能
      // 已被关掉则放弃
      let _ = cx.update_window(window, |_view, window, cx| {
        let dialog = match outcome {
          Ok(()) if success_is_visible => return,
          Ok(()) => Dialog::alert(&label, "完成"),
          Err(message) => Dialog::alert(&label, message).danger(true),
        };
        dialog::open(dialog, Rc::new(|_, _, _| {}), window, cx);
      });
    })
    .detach();
  }

  /// 执行入口：危险项（删除）先叠确认对话框（Dialog 物种，盖在页面上方；
  /// 确认后经页面实体回这里执行）。
  fn run(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
    if self.busy_idling() {
      return;
    }
    let confirm = match &command {
      Command::Unregister(name) => Some(
        Dialog::confirm(
          "删除 WSL Distribution",
          format!("删除 {name} 后，它的全部数据、设置与软件将永久丢失，且不可恢复。"),
        )
        .danger(true)
        .confirm_label("删除"),
      ),
      Command::ShutdownForce => Some(
        Dialog::confirm(
          "强制关闭全部 Distribution",
          "即使有操作在执行也终止 WSL2 虚拟机，正在写入的数据可能丢失。",
        )
        .danger(true)
        .confirm_label("强制关闭"),
      ),
      _ => None,
    };
    match confirm {
      Some(dialog) => {
        let page = cx.entity();
        dialog::open(
          dialog,
          Rc::new(move |answer, _window, cx| {
            if answer == DialogAnswer::Confirmed {
              page.update(cx, |page, cx| {
                page.execute(command.clone(), _window.window_handle(), cx)
              });
            }
          }),
          window,
          cx,
        );
      }
      None => self.execute(command, window.window_handle(), cx),
    }
  }

  /// 拉取在线可安装列表。
  fn load_online(&mut self, cx: &mut Context<Self>) {
    if self.busy_idling() {
      return;
    }
    self.online = Online::Loading;
    cx.notify();
    cx.spawn(async move |this, cx| {
      let online = cx
        .background_executor()
        .spawn(async move { zhimaterm_local::wsl::list_online() })
        .await;
      let _ = this.update(cx, |page, cx| {
        page.online = match online {
          Ok(names) if names.is_empty() => Online::Failed("没有查到可安装的 Distribution".into()),
          Ok(names) => Online::Ready(names),
          Err(message) => Online::Failed(message),
        };
        cx.notify();
      });
    })
    .detach();
  }

  /// 收起镜像下拉（backdrop 点击与 Esc 共用入口）。
  fn close_install_dropdown(&mut self, cx: &mut Context<Self>) {
    self.install_open = false;
    cx.notify();
  }

  /// 展开镜像下拉（再点收起，实际点击落在 backdrop 上也走这里之前的收起）：
  /// 候选没就绪时是禁用占位，展不开。展开时键盘高亮落在当前选中项，直接
  /// Enter 语义是「维持现状」。滚动回顶——候选浮层与上次展开共用同一滚动
  /// 句柄，残留偏移会把首屏停在半截列表。
  fn toggle_install_dropdown(&mut self, cx: &mut Context<Self>) {
    if self.busy_idling() || !matches!(self.online, Online::Ready(_)) {
      return;
    }
    self.install_open = !self.install_open;
    self.install_highlight = self.install_selected;
    if self.install_open {
      self.install_scroll.set_offset(gpui::point(px(0.), px(0.)));
    }
    cx.notify();
  }

  /// 下拉里选中镜像：记住选择并收起。
  fn pick_install_mirror(&mut self, index: usize, cx: &mut Context<Self>) {
    self.install_selected = Some(index);
    self.close_install_dropdown(cx);
  }

  /// 展开的镜像下拉里移动键盘高亮：循环，高亮项滚进视口。
  fn move_install_highlight(&mut self, step: isize, cx: &mut Context<Self>) {
    let Online::Ready(names) = &self.online else {
      return;
    };
    if names.is_empty() {
      return;
    }
    let current = self.install_highlight.or(self.install_selected).unwrap_or(0) as isize;
    let next = (current + step).rem_euclid(names.len() as isize) as usize;
    self.install_highlight = Some(next);
    self.install_scroll.scroll_to_item(next);
    cx.notify();
  }

  /// 拾取键盘高亮项（无高亮 = 维持现状）：记录选择并收起。
  fn pick_install_highlight(&mut self, cx: &mut Context<Self>) {
    if let Some(index) = self.install_highlight {
      self.install_selected = Some(index);
    }
    self.close_install_dropdown(cx);
  }

  /// 打开新增 WSL 模态：镜像下拉（异步加载候选）+ 安装设置都在模态里，
  /// 安装按钮固定在模态动作区（列表长也不滚走）。
  fn open_install_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if self.busy_idling() {
      return;
    }
    self.install_selected = None;
    self.install_highlight = None;
    self.install_open = false;
    self.install_notice = None;
    if matches!(self.online, Online::NotLoaded) {
      self.load_online(cx);
    }
    zhimaterm_base::ui::overlay::overlay_open(Box::new(WslInstallModal { page: cx.entity() }), window, cx);
  }

  /// 切换安装设置档位（no_launch / web_download / fixed_vhd）。
  fn toggle_install_flag(&mut self, flag: InstallFlag, cx: &mut Context<Self>) {
    match flag {
      InstallFlag::NoLaunch => self.install_no_launch = !self.install_no_launch,
      InstallFlag::WebDownload => self.install_web_download = !self.install_web_download,
      InstallFlag::FixedVhd => self.install_fixed_vhd = !self.install_fixed_vhd,
    }
    cx.notify();
  }

  /// 安装位置的目录选择（Windows 原生文件夹对话框，rfd）：选中后回填输入
  /// 框，仍可手改。对话框阻塞，进后台线程跑；取消则不动输入框。
  fn pick_install_location(&mut self, cx: &mut Context<Self>) {
    if self.busy_idling() {
      return;
    }
    cx.spawn(async move |this, cx| {
      let picked = cx
        .background_executor()
        .spawn(async move {
          rfd::FileDialog::new()
            .set_title("选择 Distribution 安装位置")
            .pick_folder()
        })
        .await;
      let _ = this.update(cx, |page, cx| {
        if let Some(path) = picked {
          let text = path.to_string_lossy().to_string();
          page.install_location.update(cx, |input, cx| input.set_text(&text, cx));
        }
      });
    })
    .detach();
  }

  /// 从页面安装：镜像下拉必须已选；文本项留空 = 系统默认。安装进忙碌态，
  /// 进度与结果在页面头部状态行。
  fn submit_page_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if self.busy_idling() {
      return;
    }
    let Some(selected) = self.install_selected else {
      self.install_notice = Some("先在下拉里选择要安装的 Distribution".into());
      cx.notify();
      return;
    };
    let distro = match &self.online {
      Online::Ready(names) => names.get(selected).cloned(),
      _ => None,
    };
    let Some(distro) = distro else {
      self.install_notice = Some("镜像列表已过期，请重新选择".into());
      self.install_selected = None;
      cx.notify();
      return;
    };
    let optional = |input: &Entity<TextInput>| {
      let text = input.read(cx).text().trim().to_owned();
      (!text.is_empty()).then_some(text)
    };
    // 用户名要进 root 的 shell 创建用户，白名单挡注入；非法直接打回
    // （错误在模态说明区显示——页面状态行在遮罩后看不见）
    let user = optional(&self.install_user);
    if let Some(user) = &user
      && !zhimaterm_local::wsl::is_valid_user_name(user)
    {
      self.install_notice = Some("自定义用户名不合法：字母或 _ 开头，只能含字母、数字、_ . -".into());
      cx.notify();
      return;
    }
    self.install_notice = None;
    let command = Command::Install {
      distro,
      location: optional(&self.install_location),
      name: optional(&self.install_name),
      user,
      no_launch: self.install_no_launch,
      web_download: self.install_web_download,
      fixed_vhd: self.install_fixed_vhd,
      vhd_size: optional(&self.install_vhd_size),
    };
    zhimaterm_base::ui::overlay::overlay_close_top(window, cx);
    self.execute(command, window.window_handle(), cx);
  }

  /// 打开分发版设置模态：读 `/etc/wsl.conf` 需要启动分发版，**不自动启动**
  /// ——列表里显示运行中的直接读；停着的先进「未运行」态，等用户点
  /// 「启动并读取」确认后才启动并读取。
  fn open_distro_settings(&mut self, distro: String, window: &mut Window, cx: &mut Context<Self>) {
    if self.busy_idling() {
      return;
    }
    let running = self.distros.iter().any(|entry| entry.name == distro && entry.running);
    self.distro_settings = Some(if running {
      SettingsState::Loading { distro: distro.clone() }
    } else {
      SettingsState::NotRunning { distro: distro.clone() }
    });
    zhimaterm_base::ui::overlay::overlay_open(Box::new(WslSettingsModal { page: cx.entity() }), window, cx);
    if running {
      self.load_distro_conf(distro, cx);
    }
  }

  /// 后台读取目标分发版的 `/etc/wsl.conf`（会启动分发版），读到后填入
  /// 会话；期间换目标或关模态则丢弃。
  fn load_distro_conf(&mut self, distro: String, cx: &mut Context<Self>) {
    let target = distro.clone();
    let compare = target.clone();
    cx.spawn(async move |this, cx| {
      let text = cx
        .background_executor()
        .spawn(async move { zhimaterm_local::wsl::read_wsl_conf(&target) })
        .await;
      let _ = this.update(cx, |page, cx| {
        // 仍是当前会话的分发版才应用（期间换目标/关模态则丢弃）
        let Some(SettingsState::Loading { distro: loading }) = &page.distro_settings else {
          return;
        };
        if *loading != compare {
          return;
        }
        let font = page.font.clone();
        page.distro_settings = Some(match text {
          Ok(original) => {
            let conf = zhimaterm_local::wsl::parse_wsl_conf(&original);
            let input = |placeholder: &str, value: Option<&String>, cx: &mut Context<WslManagerPage>| {
              let input = cx.new(|cx| TextInput::new(cx, placeholder, false, font.clone()));
              if let Some(value) = value {
                input.update(cx, |input, cx| input.set_text(value, cx));
              }
              input
            };
            SettingsState::Ready(Box::new(ConfSession {
              distro: distro.clone(),
              original,
              user: input("tester", conf.user_default.as_ref(), cx),
              hostname: input("Windows 计算机名", conf.network_hostname.as_ref(), cx),
              boot_command: input("service docker start", conf.boot_command.as_ref(), cx),
              boot_init_timeout: input("10000", conf.boot_init_timeout.as_ref(), cx),
              automount_root: input("/mnt/", conf.automount_root.as_ref(), cx),
              automount_options: input(
                "metadata,uid=1000,gid=1000,umask=22",
                conf.automount_options.as_ref(),
                cx,
              ),
              automount_cgroups: input("v2", conf.automount_cgroups.as_ref(), cx),
              resize: input("512GB", None, cx),
              move_to: input("D:\\wsl\\new-home", None, cx),
              conf,
            }))
          }
          Err(message) => SettingsState::Failed {
            distro: distro.clone(),
            message,
          },
        });
        cx.notify();
      });
    })
    .detach();
  }

  /// 「启动并读取」：用户确认启动停着的分发版后才进读取态（读取本身会
  /// 启动分发版）。
  fn start_distro_conf(&mut self, cx: &mut Context<Self>) {
    let Some(SettingsState::NotRunning { distro }) = &self.distro_settings else {
      return;
    };
    let distro = distro.clone();
    self.distro_settings = Some(SettingsState::Loading { distro: distro.clone() });
    cx.notify();
    self.load_distro_conf(distro, cx);
  }

  /// 翻转设置模态里的布尔键（toggle 落点，草稿即时更新为显式值）。
  fn toggle_conf_flag(&mut self, key: ConfBool, cx: &mut Context<Self>) {
    if let Some(SettingsState::Ready(session)) = &mut self.distro_settings {
      key.toggle(&mut session.conf);
      cx.notify();
    }
  }

  /// 保存设置模态：输入框与草稿合成目标 `wsl.conf`（文本留空 = 删键恢复
  /// 默认），以读取到的原文为基底合并写回；成功后提示需重启分发版生效并
  /// 关模态。
  fn submit_distro_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(SettingsState::Ready(session)) = &self.distro_settings else {
      return;
    };
    let optional = |input: &Entity<TextInput>| {
      let text = input.read(cx).text().trim().to_owned();
      (!text.is_empty()).then_some(text)
    };
    let distro = session.distro.clone();
    let original = session.original.clone();
    let distro_label = distro.clone();
    let conf = zhimaterm_local::wsl::WslConfSettings {
      boot_systemd: Some(ConfBool::BootSystemd.display(&session.conf)),
      boot_command: optional(&session.boot_command),
      boot_protect_binfmt: Some(ConfBool::BootProtectBinfmt.display(&session.conf)),
      boot_init_timeout: optional(&session.boot_init_timeout),
      network_generate_hosts: Some(ConfBool::NetworkGenerateHosts.display(&session.conf)),
      network_generate_resolv_conf: Some(ConfBool::NetworkGenerateResolvConf.display(&session.conf)),
      network_hostname: optional(&session.hostname),
      interop_enabled: Some(ConfBool::InteropEnabled.display(&session.conf)),
      interop_append_windows_path: Some(ConfBool::InteropAppendWindowsPath.display(&session.conf)),
      gpu_enabled: Some(ConfBool::GpuEnabled.display(&session.conf)),
      gpu_append_lib_path: Some(ConfBool::GpuAppendLibPath.display(&session.conf)),
      time_use_windows_timezone: Some(ConfBool::TimeUseWindowsTimezone.display(&session.conf)),
      automount_enabled: Some(ConfBool::AutomountEnabled.display(&session.conf)),
      automount_mount_fs_tab: Some(ConfBool::AutomountMountFsTab.display(&session.conf)),
      automount_root: optional(&session.automount_root),
      automount_options: optional(&session.automount_options),
      automount_ldconfig: Some(ConfBool::AutomountLdconfig.display(&session.conf)),
      automount_cgroups: optional(&session.automount_cgroups),
      user_default: optional(&session.user),
    };
    self.distro_settings = None;
    zhimaterm_base::ui::overlay::overlay_close_top(window, cx);
    let window_handle = window.window_handle();
    cx.spawn(async move |_this, cx| {
      let outcome = cx
        .background_executor()
        .spawn(async move {
          zhimaterm_local::wsl::write_wsl_conf(&distro, &zhimaterm_local::wsl::merge_wsl_conf(&original, &conf))
        })
        .await;
      // 页面状态已无变化（编辑态在提交时收掉），结果直接以弹窗展示；
      // 窗口可能已被关掉则放弃
      let _ = cx.update_window(window_handle, |_view, window, cx| {
        let dialog = match outcome {
          Ok(()) => Dialog::alert(
            "保存设置",
            format!("已保存 {distro_label} 的设置；需要终止并重新打开该 Distribution 后生效"),
          ),
          Err(message) => Dialog::alert("保存设置", message).danger(true),
        };
        dialog::open(dialog, Rc::new(|_, _, _| {}), window, cx);
      });
    })
    .detach();
  }

  /// 关闭分发版设置模态（不保存）。
  fn close_distro_settings(&mut self, cx: &mut Context<Self>) {
    self.distro_settings = None;
    cx.notify();
  }

  /// 管理操作：调整磁盘大小（`wsl --manage --resize`，格式错误由 wsl.exe
  /// 校验后透传）。
  fn submit_resize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(SettingsState::Ready(session)) = &self.distro_settings else {
      return;
    };
    let size = session.resize.read(cx).text().trim().to_owned();
    let name = session.distro.clone();
    if size.is_empty() {
      let dialog = Dialog::alert("调整磁盘大小", "目标大小不能为空（如 512GB）");
      dialog::open(dialog, Rc::new(|_, _, _| {}), window, cx);
      return;
    }
    self.run(Command::Resize { name, size }, window, cx);
  }

  /// 管理操作：移动分发版到新位置（目标非空才弹确认，耗时操作）。
  fn submit_move(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(SettingsState::Ready(session)) = &self.distro_settings else {
      return;
    };
    let location = session.move_to.read(cx).text().trim().to_owned();
    let name = session.distro.clone();
    if location.is_empty() {
      let dialog = Dialog::alert("移动 Distribution", "目标位置不能为空");
      dialog::open(dialog, Rc::new(|_, _, _| {}), window, cx);
      return;
    }
    let dialog = Dialog::confirm(
      format!("移动 {name}"),
      format!("把 {name} 的磁盘移动到 {location}，Distribution 需处于停止状态；耗时取决于数据量与目标盘速度。"),
    )
    .confirm_label("移动");
    let page = cx.entity();
    dialog::open(
      dialog,
      Rc::new(move |answer, _window, cx| {
        if answer == DialogAnswer::Confirmed {
          page.update(cx, |page, cx| {
            page.execute(
              Command::Move {
                name: name.clone(),
                location: location.clone(),
              },
              _window.window_handle(),
              cx,
            )
          });
        }
      }),
      window,
      cx,
    );
  }

  /// 管理操作：稀疏 VHD 开关（点即执行）。
  fn set_sparse(&mut self, sparse: bool, window: &mut Window, cx: &mut Context<Self>) {
    let Some(SettingsState::Ready(session)) = &self.distro_settings else {
      return;
    };
    let name = session.distro.clone();
    self.run(Command::SetSparse { name, sparse }, window, cx);
  }

  /// 拉取 WSL 本体版本信息（只读展示，取前两行：WSL 版本 + 内核版本）。
  fn load_version(&mut self, cx: &mut Context<Self>) {
    cx.spawn(async move |this, cx| {
      let version = cx
        .background_executor()
        .spawn(async move { zhimaterm_local::wsl::version() })
        .await;
      let _ = this.update(cx, |page, cx| {
        page.wsl_version = version.ok().map(|text| {
          text
            .lines()
            .take(2)
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
        });
        cx.notify();
      });
    })
    .detach();
  }

  // -----------------------------------------------------------------------
  // 渲染
  // -----------------------------------------------------------------------

  /// 行操作小按钮：比区块按钮小一号；`enabled=false`（命令运行中）只降
  /// 视觉不触发动作。
  fn row_button(
    tokens: &UiTokens,
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    danger: bool,
    enabled: bool,
    on_click: RowAction,
  ) -> gpui::Stateful<gpui::Div> {
    panel::clickable(id, move |_, window, cx| {
      if enabled {
        on_click(window, cx)
      }
    })
    .px(px(7.))
    .py(px(2.))
    .rounded(px(4.))
    .text_size(ui_rems(10.))
    .border_1()
    .border_color(gpui::rgb(if enabled {
      if danger {
        tokens.border_subtle
      } else {
        tokens.border_default
      }
    } else {
      tokens.border_subtle
    }))
    .text_color(gpui::rgb(match (enabled, danger) {
      (false, _) => tokens.fg_muted,
      (true, false) => tokens.fg_secondary,
      (true, true) => tokens.danger,
    }))
    .hover(|style| {
      if enabled {
        style.border_color(gpui::rgb(if danger { tokens.danger } else { tokens.border_hover }))
      } else {
        style
      }
    })
    .child(label.to_owned())
  }
}

/// 页面焦点根走 gpui 的 Focusable 约定：外壳开页面时把焦点交给它，
/// Esc 的页面内裁决才能收到键。
impl gpui::Focusable for WslManagerPage {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus.clone()
  }
}

/// 管理模态：列表 + 维护动作的浮层包装。页面实体继续承载全部状态与 wsl.exe
/// 编排，模态只负责 Card 组装（标题、内容、动作行、Esc）。
pub(crate) struct WslManagerModal {
  pub(crate) page: Entity<WslManagerPage>,
}

impl WslManagerModal {
  /// 内容：忙碌状态（有命令在跑时）+ 分发版列表。版本信息在标题行。
  fn content(&self, cx: &App) -> AnyElement {
    let tokens = theme::tokens(cx);
    let page = self.page.read(cx);
    let mut body = div().flex().flex_col().gap(px(10.));
    if let Some(label) = &page.busy {
      body = body.child(
        div()
          .text_size(ui_rems(11.))
          .text_color(gpui::rgb(tokens.fg_secondary))
          .child(format!("正在{label}")),
      );
    }
    let list = page.render_distros(&tokens, &self.page);
    body.child(list).into_any_element()
  }

  /// 动作行：左侧新增与维护按钮，右侧「关闭」。
  fn actions(&self, cx: &App) -> AnyElement {
    let tokens = theme::tokens(cx);
    let page = &self.page;
    let run = |id: &'static str, label: &'static str, command: Command| {
      let page = page.clone();
      panel::secondary_button(&tokens, id, label, None, move |_, window, cx| {
        page.update(cx, |page, cx| page.run(command.clone(), window, cx))
      })
    };
    let install = {
      let page = page.clone();
      panel::primary_button(&tokens, "wsl-online-open", "新增 WSL", None, move |_, window, cx| {
        page.update(cx, |page, cx| page.open_install_modal(window, cx))
      })
    };
    // 强制关闭是危险动作：红底危险按钮（与同排按钮同规格）+ 危险确认对话
    // 框（可能数据丢失）
    let force = {
      let page = page.clone();
      panel::danger_button(
        &tokens,
        "wsl-shutdown-force",
        "强制关闭",
        None,
        move |_, window, cx| page.update(cx, |page, cx| page.run(Command::ShutdownForce, window, cx)),
      )
    };
    let refresh = {
      let page = page.clone();
      panel::secondary_button(&tokens, "wsl-refresh", "刷新列表", None, move |_, _, cx| {
        page.update(cx, |page, cx| page.refresh(cx))
      })
    };
    let close = panel::secondary_button(&tokens, "wsl-manager-close", "关闭", Some("esc"), |_, window, cx| {
      zhimaterm_base::ui::overlay::overlay_close_top(window, cx);
    });
    // 按职责分三组，组间竖线：创建 │ 批量维护 │ 危险动作
    let divider = || zhimaterm_base::ui::vertical_divider(&tokens).into_any_element();
    panel::dialog_actions(
      [
        install.into_any_element(),
        divider(),
        run("wsl-shutdown", "全部关闭", Command::Shutdown).into_any_element(),
        run("wsl-update", "更新 WSL", Command::Update).into_any_element(),
        refresh.into_any_element(),
        divider(),
        force.into_any_element(),
      ],
      Some(close.into_any_element()),
    )
    .into_any_element()
  }
}

impl WslManagerModal {
  /// 标题行：名称居左，版本信息（只读）小字居右。
  fn header(&self, cx: &App) -> gpui::Div {
    let tokens = theme::tokens(cx);
    let version = self.page.read(cx).wsl_version.clone();
    div()
      .flex()
      .items_center()
      .gap(px(10.))
      .min_w(px(0.))
      .child(zhimaterm_base::ui::card::title(&tokens, "WSL 管理"))
      .children(version.map(|text| {
        div()
          .min_w(px(0.))
          .flex_1()
          .truncate()
          .text_size(ui_rems(10.))
          .text_color(gpui::rgb(tokens.fg_muted))
          .child(text)
      }))
  }
}

impl ModalView for WslManagerModal {
  fn species(&self) -> Species {
    Species::Modal
  }

  /// 卡片组装覆写：只为加宽（滚动与八成视高封顶是 Card 默认行为）。
  fn card(&self, _window: &Window, cx: &App) -> gpui::Div {
    let tokens = theme::tokens(cx);
    zhimaterm_base::ui::card::Card::new(&tokens)
      .header(self.header(cx))
      .content(self.content(cx))
      .actions(self.actions(cx))
      .build()
      .w(ui_rems(620.))
      .min_h(px(320.))
  }

  fn title(&self) -> Option<String> {
    None
  }

  fn content(&self, _window: &Window, cx: &App) -> AnyElement {
    self.content(cx)
  }

  fn actions(&self, cx: &App) -> AnyElement {
    WslManagerModal::actions(self, cx)
  }

  fn key_action(&self, key: &Keystroke, _cx: &App) -> KeyHandling {
    if bare_key(key) && key.key.eq_ignore_ascii_case("escape") {
      return KeyHandling::Close;
    }
    KeyHandling::None
  }

  fn on_dismiss(&self) -> KeyHandling {
    KeyHandling::Close
  }

  fn focus_ring(&self, cx: &App) -> Vec<gpui::FocusHandle> {
    vec![gpui::Focusable::focus_handle(self.page.read(cx), cx)]
  }

  fn as_any(&self) -> &dyn std::any::Any {
    self
  }

  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl WslManagerPage {
  /// 分发版列表区块。
  fn render_distros(&self, tokens: &UiTokens, page: &Entity<Self>) -> gpui::Div {
    let busy = self.busy_idling();
    let mut body = div().flex().flex_col().gap(px(6.));
    if self.distros.is_empty() {
      body = body.child(
        div()
          .text_size(ui_rems(11.))
          .text_color(gpui::rgb(tokens.fg_muted))
          .child("本机还没有已安装的 Distribution——点下方「新增 WSL」从官方候选安装。"),
      );
      return body;
    }

    let rows = self.distros.iter().enumerate().map(|(index, distro)| {
      let name = distro.name.clone();
      // 副行：家族（首字母大写）+ 系统版本 + WSL 大版本 + 运行状态 + 磁盘占用
      let mut parts: Vec<String> = Vec::new();
      if let Some(flavor) = &distro.flavor {
        let mut characters = flavor.chars();
        parts.push(
          characters
            .next()
            .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
            .unwrap_or_else(|| flavor.clone()),
        );
      }
      if let Some(version) = &distro.os_version {
        parts.push(version.clone());
      }
      if let Some(version) = distro.wsl_version {
        parts.push(format!("WSL{version}"));
      }
      if let Some(size) = distro.size_bytes {
        parts.push(human_bytes(size));
      }
      // 安装路径放副行末尾：诊断「磁盘在哪」用，长路径就地换行
      if let Some(path) = &distro.base_path {
        parts.push(path.clone());
      }
      // 行主体点击开会话；行尾操作按钮各自拦截点击
      let pick = self.on_pick.clone();
      let line = div()
        .flex()
        .items_center()
        .gap(px(12.))
        .child(
          panel::clickable(("wsl-row", index), move |_, window, cx| pick(&name, window, cx))
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
              div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                  div()
                    .text_size(ui_rems(12.))
                    .text_color(gpui::rgb(tokens.fg_value))
                    .child(distro.name.clone()),
                )
                // 默认 tag 紧贴名称右侧，不进行尾按钮区
                .children(distro.is_default.then(|| {
                  div()
                    .flex_none()
                    .px(px(5.))
                    .py(px(1.))
                    .rounded(px(3.))
                    .bg(gpui::rgb(tokens.bg_success))
                    .text_color(gpui::rgb(tokens.accent))
                    .text_size(ui_rems(9.))
                    .child("默认")
                }))
                // 运行状态 chip：一眼可辨（运行中绿 / 已停止灰）
                .child({
                  let (label, bg, fg) = if distro.running {
                    ("运行中", tokens.bg_success, tokens.accent)
                  } else {
                    ("已停止", tokens.bg_row, tokens.fg_muted)
                  };
                  div()
                    .flex_none()
                    .px(px(5.))
                    .py(px(1.))
                    .rounded(px(3.))
                    .bg(gpui::rgb(bg))
                    .text_color(gpui::rgb(fg))
                    .text_size(ui_rems(9.))
                    .child(label)
                }),
            )
            .children((!parts.is_empty()).then(|| {
              div()
                .text_size(ui_rems(10.))
                .text_color(gpui::rgb(tokens.fg_muted))
                .child(parts.join(" · "))
            })),
        )
        .child(self.render_row_actions(tokens, index, distro, busy, page));

      // 行卡片：主体一行
      div()
        .w_full()
        .px(px(10.))
        .py(px(8.))
        .rounded(px(6.))
        .border_1()
        .border_color(gpui::rgb(tokens.border_subtle))
        .bg(gpui::rgb(tokens.bg_row))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(line)
    });

    div().flex().flex_col().gap(px(6.)).children(rows)
  }

  /// 一行分发版的操作按钮组。
  fn render_row_actions(
    &self,
    tokens: &UiTokens,
    index: usize,
    distro: &zhimaterm_local::wsl::WslDistro,
    busy: bool,
    page: &Entity<Self>,
  ) -> gpui::Div {
    // 命令执行动作（危险项的确认对话框编排也在 run 里）
    let run: RunAction = {
      let page = page.clone();
      Rc::new(move |command, window, cx| page.update(cx, |page, cx| page.run(command, window, cx)))
    };
    let mut buttons = div().flex().flex_none().items_center().gap(px(4.));
    if !distro.is_default {
      let (run, name) = (run.clone(), distro.name.clone());
      buttons = buttons.child(Self::row_button(
        tokens,
        ("wsl-set-default", index),
        "默认",
        false,
        !busy,
        Rc::new(move |window, cx| run(Command::SetDefault(name.clone()), window, cx)),
      ));
    }
    if distro.running {
      let (run, name) = (run.clone(), distro.name.clone());
      buttons = buttons.child(Self::row_button(
        tokens,
        ("wsl-terminate", index),
        "终止",
        false,
        !busy,
        Rc::new(move |window, cx| run(Command::Terminate(name.clone()), window, cx)),
      ));
    }
    {
      let page = page.clone();
      let name = distro.name.clone();
      buttons = buttons.child(Self::row_button(
        tokens,
        ("wsl-edit-user", index),
        "设置",
        false,
        !busy,
        Rc::new(move |window, cx| page.update(cx, |page, cx| page.open_distro_settings(name.clone(), window, cx))),
      ));
    }
    let (run, name) = (run, distro.name.clone());
    buttons = buttons.child(Self::row_button(
      tokens,
      ("wsl-unregister", index),
      "删除",
      true,
      !busy,
      Rc::new(move |window, cx| run(Command::Unregister(name.clone()), window, cx)),
    ));
    buttons
  }

  /// 镜像选择：select 触发行**始终占位**（未加载/失败时降透明度禁用，
  /// 标签说明当前状态），展开的候选浮层经模态 `floaters` 挂浮层栈根层
  /// （不受卡片裁剪，ADR-044）；失败态在下方给错误与重试。加载在模态
  /// 打开时触发。
  fn render_mirror_select(&self, tokens: &UiTokens, page: &Entity<Self>) -> gpui::Div {
    let (label, ready) = match (&self.online, self.install_selected) {
      (Online::Ready(names), _) => {
        // 未选择（或下标过期）给占位标签；有过期下标时清掉，安装守卫兜底
        let label = match self.install_selected.filter(|index| *index < names.len()) {
          Some(index) => names[index].clone(),
          None => format!("点击选择 Distribution（共 {} 个）", names.len()),
        };
        (label, true)
      }
      (Online::NotLoaded | Online::Loading, _) => ("正在加载镜像列表".to_owned(), false),
      (Online::Failed(_), _) => ("镜像列表加载失败".to_owned(), false),
    };
    let toggle = page.clone();
    let select = plain_select_row(
      tokens,
      "wsl-mirror-select",
      &label,
      self.install_open,
      &self.install_bounds,
      move |_, _, cx| {
        toggle.update(cx, |page, cx| page.toggle_install_dropdown(cx));
      },
    );
    let mut body = div().flex().flex_col().gap(px(6.)).child(if ready {
      select.into_any_element()
    } else {
      select.opacity(0.5).into_any_element()
    });
    if let Online::Failed(message) = &self.online {
      let retry = page.clone();
      body = body
        .child(panel::error_line(tokens, message))
        .child(panel::secondary_button(
          tokens,
          "wsl-mirror-retry",
          "重试",
          None,
          move |_, _, cx| retry.update(cx, |page, cx| page.load_online(cx)),
        ));
    }
    body
  }
}

/// 设置键的官网解释（learn.microsoft.com/windows/wsl/wsl-config，中文口径）。
/// 每个设置项下方展示，随实现一同维护。
mod conf_docs {
  pub const BOOT_SYSTEMD: &str =
    "在该 Distribution 中使用 systemd 作为 init 系统。需要 WSL 0.67.6 或更高版本，默认值取决于 Distribution。";
  pub const BOOT_COMMAND: &str = "该 Distribution 每次启动时以 root 身份执行的命令，例如 service docker start。其输出不会显示，执行失败也不会终止启动。";
  pub const BOOT_PROTECT_BINFMT: &str = "开启时，WSL 会保留用于 Windows 互操作的 binfmt 注册，systemd 无法将其替换。关闭后注册交由 systemd 管理，可能导致 Distribution 中无法运行 Windows 程序。需要 Windows 11 或 Server 2022。";
  pub const BOOT_INIT_TIMEOUT: &str =
    "WSL 等待 systemd 初始化完成的毫秒数，默认 10000（10 秒）。systemd 服务较多、启动超时的 Distribution 可以调大。";
  pub const NETWORK_GENERATE_HOSTS: &str = "让 WSL 根据 Windows 主机生成 /etc/hosts（主机名与 IP 地址的静态映射）。";
  pub const NETWORK_GENERATE_RESOLV_CONF: &str =
    "让 WSL 使用 Windows 主机的 DNS 服务器生成 /etc/resolv.conf。若要使用自己的 DNS 配置，请关闭此项。";
  pub const NETWORK_HOSTNAME: &str = "该 Distribution 的主机名。默认为 Windows 计算机名。";
  pub const INTEROP_ENABLED: &str = "允许该 Distribution 启动 Windows 可执行文件。需要 Windows 10 1809 或更高版本。";
  pub const INTEROP_APPEND_WINDOWS_PATH: &str =
    "将 Windows 的 PATH 追加到 Linux 的 PATH，以便按名称调用 Windows 程序。";
  pub const GPU_ENABLED: &str = "允许 Linux 应用程序通过准虚拟化访问 Windows GPU。";
  pub const GPU_APPEND_LIB_PATH: &str =
    "GPU 支持开启时，把 /usr/lib/wsl/lib 加入 $PATH，使 Linux 应用能找到 WSL 提供的 GPU 库。";
  pub const TIME_USE_WINDOWS_TIMEZONE: &str = "使该 Distribution 的时区与 Windows 中设置的时区保持一致并同步。";
  pub const AUTOMOUNT_ENABLED: &str =
    "自动将 Windows 固定驱动器（如 C:/、D:/）挂载到挂载根目录下；关闭后仍可手动或通过 fstab 挂载。";
  pub const AUTOMOUNT_MOUNT_FS_TAB: &str = "Distribution 启动时处理 /etc/fstab 中的条目。";
  pub const AUTOMOUNT_ROOT: &str = "Windows 驱动器挂载到的目录。默认 /mnt/，因此 C 盘会挂载到 /mnt/c。";
  pub const AUTOMOUNT_OPTIONS: &str = "Windows 驱动器的 DrvFs 选项，以逗号分隔：uid、gid、umask、fmask、dmask、metadata 和 case。若同时设置了 metadata，umask、fmask 和 dmask 不会生效。示例：metadata,uid=1000,gid=1000,umask=22";
  pub const AUTOMOUNT_LDCONFIG: &str =
    "把 Windows 提供的 GPU 库加入动态链接器搜索路径并运行 ldconfig。仅在 WSL2 且开启 GPU 支持时生效。";
  pub const AUTOMOUNT_CGROUPS: &str = "选择 WSL2 挂载的 cgroup 层级：v1 或 v2，默认 v2。仅在容器工具需要旧层级时才改。";
  pub const USER_DEFAULT: &str = "以该用户身份启动首次 WSL 会话。留空 = 使用 Distribution 首次运行时创建的初始用户。";
}

/// 分发版设置模态（Modal 物种）：分组展示 `/etc/wsl.conf` 的 GUI 管理键
/// 与 `wsl --manage` 的管理操作。状态归页面实体（SettingsState），模态
/// 只是视图；保存经页面实体回 `submit_distro_settings`。
struct WslSettingsModal {
  page: Entity<WslManagerPage>,
}

impl WslSettingsModal {
  /// 「默认」徽标：灰底描边小字，挂在设置项名称右侧，表示当前显示的值是
  /// WSL 默认值、还没有被显式设置；点击/输入转为显式后徽标消失。
  fn default_badge(tokens: &UiTokens) -> gpui::Div {
    div()
      .flex_none()
      .px(px(5.))
      .py(px(1.))
      .rounded(px(4.))
      .bg(gpui::rgb(tokens.bg_row))
      .border_1()
      .border_color(gpui::rgb(tokens.border_subtle))
      .text_size(ui_rems(9.))
      .text_color(gpui::rgb(tokens.fg_muted))
      .child("默认")
  }

  /// 自绘 toggle：胶囊 + 圆点，激活时 accent 底、圆点居右。样式只由当前
  /// 值决定——未显式设置时展示的默认值同样用正常实色（可点性直观），
  /// 「这是默认值」由名称旁的徽标标注。
  fn toggle(tokens: &UiTokens, id: &'static str, active: bool, on_click: RowAction) -> gpui::Stateful<gpui::Div> {
    panel::clickable(id, move |_, window, cx| on_click(window, cx))
      .w(px(30.))
      .h(px(16.))
      .flex_none()
      .rounded(px(8.))
      .bg(gpui::rgb(if active { tokens.bg_accent } else { tokens.bg_row }))
      .border_1()
      .border_color(gpui::rgb(if active {
        tokens.bg_accent
      } else {
        tokens.border_default
      }))
      .flex()
      .flex_none()
      .items_center()
      .child(
        div()
          .mx(px(2.))
          .size(px(10.))
          .rounded_full()
          .bg(gpui::rgb(if active { tokens.accent } else { tokens.fg_muted }))
          .when(active, |dot| dot.ml_auto()),
      )
  }

  /// 一项布尔设置：开关 + 名称一行，下方官网解释与设置状态行。未显式设置
  /// 时名称旁挂「默认」徽标（开关按默认值展示、样式不变），点击即转为
  /// 显式设置、徽标消失。
  fn bool_item(
    tokens: &UiTokens,
    id: &'static str,
    label: &'static str,
    description: &'static str,
    value: bool,
    is_set: bool,
    on_toggle: RowAction,
  ) -> gpui::Div {
    let mut head = div()
      .flex()
      .items_center()
      .gap(px(8.))
      .child(Self::toggle(tokens, id, value, on_toggle))
      .child(
        div()
          .text_size(ui_rems(11.))
          .text_color(gpui::rgb(tokens.fg))
          .child(label.to_owned()),
      );
    if !is_set {
      head = head.child(Self::default_badge(tokens));
    }
    div()
      .flex()
      .flex_col()
      .gap(px(2.))
      .child(head)
      .child(
        div()
          .text_size(ui_rems(10.))
          .text_color(gpui::rgb(tokens.fg_muted))
          .child(description.to_owned()),
      )
      .child(
        div()
          .text_size(ui_rems(10.))
          .text_color(gpui::rgb(tokens.fg_faint))
          .child(if is_set {
            "已设置（保存后写入 wsl.conf）"
          } else {
            "未设置 — 默认值临时生效，点击覆盖"
          }),
      )
  }

  /// 一项文本设置：输入框 + 官网解释。未显式设置时标签右侧挂「默认」徽标
  /// （与布尔项同款）；已设置提示独占一行（保存后才落键）。
  fn text_item(
    tokens: &UiTokens,
    label: &'static str,
    description: &'static str,
    is_set: bool,
    input: &Entity<TextInput>,
    cx: &App,
  ) -> gpui::Div {
    let hint = (!is_set).then(|| Self::default_badge(tokens).into_any_element());
    let mut item = div()
      .flex()
      .flex_col()
      .gap(px(2.))
      .child(input_field_with_hint(tokens, cx, label, hint, input))
      .child(
        div()
          .text_size(ui_rems(10.))
          .text_color(gpui::rgb(tokens.fg_muted))
          .child(description.to_owned()),
      );
    if is_set {
      item = item.child(
        div()
          .text_size(ui_rems(10.))
          .text_color(gpui::rgb(tokens.fg_faint))
          .child("已设置（保存后写入 wsl.conf）"),
      );
    }
    item
  }

  /// 分组容器：组标题 + 条目。
  fn group(tokens: &UiTokens, title: &'static str, body: gpui::Div) -> gpui::Div {
    div()
      .flex()
      .flex_col()
      .gap(px(8.))
      .child(panel::section_header(tokens, title, None))
      .child(body)
  }
}

impl zhimaterm_base::ui::overlay::ModalView for WslSettingsModal {
  fn species(&self) -> zhimaterm_base::ui::overlay::Species {
    zhimaterm_base::ui::overlay::Species::Modal
  }

  /// 卡片组装覆写：只为加宽（滚动与八成视高封顶是 Card 默认行为）。
  /// 标题带分发版名（card 有 cx 才能读页面状态，title() 拿不到）。
  fn card(&self, window: &Window, cx: &App) -> gpui::Div {
    let tokens = theme::tokens(cx);
    let page = self.page.read(cx);
    let distro = match &page.distro_settings {
      Some(SettingsState::NotRunning { distro })
      | Some(SettingsState::Loading { distro })
      | Some(SettingsState::Failed { distro, .. }) => distro,
      Some(SettingsState::Ready(session)) => &session.distro,
      None => "",
    };
    zhimaterm_base::ui::card::Card::new(&tokens)
      .header(zhimaterm_base::ui::card::title(
        &tokens,
        &format!("Distribution 设置 — {distro}"),
      ))
      .content(self.content(window, cx))
      .actions(self.actions(cx))
      .build()
      .w(ui_rems(460.))
      .min_h(px(280.))
  }

  fn title(&self) -> Option<String> {
    None
  }

  fn content(&self, _window: &Window, cx: &App) -> AnyElement {
    let tokens = theme::tokens(cx);
    let page = self.page.read(cx);
    let busy = page.busy_idling();
    let body = match &page.distro_settings {
      Some(SettingsState::NotRunning { distro }) => div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .text_size(ui_rems(11.))
        .child(format!("{distro} 当前未运行。"))
        .child(
          div()
            .text_size(ui_rems(10.))
            .text_color(gpui::rgb(tokens.fg_muted))
            .child("读取 /etc/wsl.conf 需要启动 Distribution（通常需要数秒）。点下方「启动并读取」确认后开始；只想改配置、不想现在启动的话，先关闭这个窗口。"),
        ),
      None | Some(SettingsState::Loading { .. }) => div()
        .text_size(ui_rems(11.))
        .text_color(gpui::rgb(tokens.fg_muted))
        .child("正在读取 /etc/wsl.conf"),
      Some(SettingsState::Failed { message, .. }) => div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(panel::error_line(&tokens, message)),
      Some(SettingsState::Ready(session)) => self.render_ready(&tokens, session, busy, cx),
    };
    // 滚动与内边距归 Card 的默认内容容器，这里只排内容本身
    div()
      .flex()
      .flex_col()
      .gap(px(14.))
      .when_some(
        // 未运行提示态没有「配置修改后需重启」的上下文，说明行只在其余态给
        (!matches!(page.distro_settings, Some(SettingsState::NotRunning { .. }))).then_some(()),
        |content, _| {
          content.child(
            div()
              .flex_none()
              .text_size(ui_rems(10.))
              .text_color(gpui::rgb(tokens.fg_muted))
              .child("以下为 /etc/wsl.conf 的 Distribution 级配置；修改保存后需终止并重新打开 Distribution 才生效。来源：learn.microsoft.com/windows/wsl/wsl-config"),
          )
        },
      )
      .child(body)
      .into_any_element()
  }

  fn actions(&self, cx: &App) -> AnyElement {
    let tokens = theme::tokens(cx);
    let page = self.page.read(cx);
    let busy = page.busy_idling();
    let ready = matches!(page.distro_settings, Some(SettingsState::Ready(_)));
    let not_running = matches!(page.distro_settings, Some(SettingsState::NotRunning { .. }));
    let save = self.page.clone();
    let close = self.page.clone();
    // 未运行时主动作是「启动并读取」（读取会启动分发版，须用户确认）；
    // 其余态是「保存设置」
    let (primary_id, primary_label) = if not_running {
      ("wsl-settings-start", "启动并读取")
    } else {
      ("wsl-settings-save", "保存设置")
    };
    panel::dialog_actions(
      [
        panel::primary_button(&tokens, primary_id, primary_label, None, move |_, window, cx| {
          if busy {
            return;
          }
          if not_running {
            save.update(cx, |page, cx| page.start_distro_conf(cx));
          } else if ready {
            save.update(cx, |page, cx| page.submit_distro_settings(window, cx))
          }
        })
        .into_any_element(),
      ],
      Some(
        // 关闭 = 页面侧丢会话态（迟到的 conf 回写因此不落）+ 关浮层栈顶；
        // 此前误只清页面态，浮层不出栈，模态关不掉
        panel::secondary_button(
          &tokens,
          "wsl-settings-close",
          "关闭",
          Some("esc"),
          move |_, window, cx| {
            close.update(cx, |page, cx| page.close_distro_settings(cx));
            zhimaterm_base::ui::overlay::overlay_close_top(window, cx);
          },
        )
        .into_any_element(),
      ),
    )
    .into_any_element()
  }

  fn as_any(&self) -> &dyn std::any::Any {
    self
  }

  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl WslSettingsModal {
  /// Ready 态：wsl.conf 分组设置 + 管理操作。
  fn render_ready(&self, tokens: &UiTokens, session: &ConfSession, busy: bool, cx: &App) -> gpui::Div {
    let conf = &session.conf;
    let page = self.page.clone();
    // 开关项装配：读草稿值（未设置回默认值）与已设置标记，翻转回页面实体
    let toggle_item = |tokens: &UiTokens,
                       key: ConfBool,
                       id: &'static str,
                       label: &'static str,
                       description: &'static str,
                       page: &Entity<WslManagerPage>| {
      let (value, is_set) = (key.display(conf), key.is_set(conf));
      let on_toggle: RowAction = {
        let page = page.clone();
        Rc::new(move |_, cx| page.update(cx, |page, cx| page.toggle_conf_flag(key, cx)))
      };
      Self::bool_item(tokens, id, label, description, value, is_set, on_toggle)
    };
    let manage_button = |tokens: &UiTokens, id: &'static str, label: &'static str, on_click: RowAction| {
      WslManagerPage::row_button(tokens, id, label, false, !busy, on_click)
    };

    let boot = div()
      .flex()
      .flex_col()
      .gap(px(10.))
      .child(toggle_item(
        tokens,
        ConfBool::BootSystemd,
        "wsl-conf-systemd",
        "启用 systemd",
        conf_docs::BOOT_SYSTEMD,
        &page,
      ))
      .child(Self::text_item(
        tokens,
        "启动命令",
        conf_docs::BOOT_COMMAND,
        session.conf.boot_command.is_some(),
        &session.boot_command,
        cx,
      ))
      .child(Self::text_item(
        tokens,
        "systemd 初始化超时（毫秒）",
        conf_docs::BOOT_INIT_TIMEOUT,
        session.conf.boot_init_timeout.is_some(),
        &session.boot_init_timeout,
        cx,
      ))
      .child(toggle_item(
        tokens,
        ConfBool::BootProtectBinfmt,
        "wsl-conf-protect-binfmt",
        "保护 binfmt 注册",
        conf_docs::BOOT_PROTECT_BINFMT,
        &page,
      ));
    let network = div()
      .flex()
      .flex_col()
      .gap(px(10.))
      .child(toggle_item(
        tokens,
        ConfBool::NetworkGenerateHosts,
        "wsl-conf-generate-hosts",
        "生成 /etc/hosts",
        conf_docs::NETWORK_GENERATE_HOSTS,
        &page,
      ))
      .child(toggle_item(
        tokens,
        ConfBool::NetworkGenerateResolvConf,
        "wsl-conf-generate-resolv-conf",
        "生成 /etc/resolv.conf",
        conf_docs::NETWORK_GENERATE_RESOLV_CONF,
        &page,
      ))
      .child(Self::text_item(
        tokens,
        "主机名",
        conf_docs::NETWORK_HOSTNAME,
        session.conf.network_hostname.is_some(),
        &session.hostname,
        cx,
      ));
    let interop = div()
      .flex()
      .flex_col()
      .gap(px(10.))
      .child(toggle_item(
        tokens,
        ConfBool::InteropEnabled,
        "wsl-conf-interop",
        "运行 Windows 程序",
        conf_docs::INTEROP_ENABLED,
        &page,
      ))
      .child(toggle_item(
        tokens,
        ConfBool::InteropAppendWindowsPath,
        "wsl-conf-append-path",
        "追加 Windows PATH",
        conf_docs::INTEROP_APPEND_WINDOWS_PATH,
        &page,
      ));
    let automount = div()
      .flex()
      .flex_col()
      .gap(px(10.))
      .child(toggle_item(
        tokens,
        ConfBool::AutomountEnabled,
        "wsl-conf-automount",
        "挂载 Windows 驱动器",
        conf_docs::AUTOMOUNT_ENABLED,
        &page,
      ))
      .child(toggle_item(
        tokens,
        ConfBool::AutomountMountFsTab,
        "wsl-conf-mount-fstab",
        "处理 /etc/fstab",
        conf_docs::AUTOMOUNT_MOUNT_FS_TAB,
        &page,
      ))
      .child(Self::text_item(
        tokens,
        "挂载根目录",
        conf_docs::AUTOMOUNT_ROOT,
        session.conf.automount_root.is_some(),
        &session.automount_root,
        cx,
      ))
      .child(Self::text_item(
        tokens,
        "挂载选项",
        conf_docs::AUTOMOUNT_OPTIONS,
        session.conf.automount_options.is_some(),
        &session.automount_options,
        cx,
      ))
      .child(toggle_item(
        tokens,
        ConfBool::AutomountLdconfig,
        "wsl-conf-ldconfig",
        "GPU 库加入链接路径",
        conf_docs::AUTOMOUNT_LDCONFIG,
        &page,
      ))
      .child(Self::text_item(
        tokens,
        "cgroup 层级（v1 / v2）",
        conf_docs::AUTOMOUNT_CGROUPS,
        session.conf.automount_cgroups.is_some(),
        &session.automount_cgroups,
        cx,
      ));
    let gpu = div()
      .flex()
      .flex_col()
      .gap(px(10.))
      .child(toggle_item(
        tokens,
        ConfBool::GpuEnabled,
        "wsl-conf-gpu",
        "GPU 加速",
        conf_docs::GPU_ENABLED,
        &page,
      ))
      .child(toggle_item(
        tokens,
        ConfBool::GpuAppendLibPath,
        "wsl-conf-gpu-lib-path",
        "GPU 库加入 PATH",
        conf_docs::GPU_APPEND_LIB_PATH,
        &page,
      ));
    let time = div().flex().flex_col().gap(px(10.)).child(toggle_item(
      tokens,
      ConfBool::TimeUseWindowsTimezone,
      "wsl-conf-timezone",
      "使用 Windows 时区",
      conf_docs::TIME_USE_WINDOWS_TIMEZONE,
      &page,
    ));
    let user = div().flex().flex_col().gap(px(10.)).child(Self::text_item(
      tokens,
      "默认用户",
      conf_docs::USER_DEFAULT,
      session.conf.user_default.is_some(),
      &session.user,
      cx,
    ));
    // 管理操作（wsl --manage）：每项独立执行，不等保存
    let (sparse_on, sparse_off, resize, move_to) = (
      self.page.clone(),
      self.page.clone(),
      self.page.clone(),
      self.page.clone(),
    );
    let manage = div()
      .flex()
      .flex_col()
      .gap(px(10.))
      .child(
        div()
          .flex()
          .items_center()
          .gap(px(6.))
          .child(
            div()
              .text_size(ui_rems(10.))
              .text_color(gpui::rgb(tokens.fg_muted))
              .child("稀疏 VHD（自动回收磁盘空间）："),
          )
          .child(manage_button(
            tokens,
            "wsl-manage-sparse-on",
            "设为稀疏",
            Rc::new(move |window, cx| sparse_on.update(cx, |page, cx| page.set_sparse(true, window, cx))),
          ))
          .child(manage_button(
            tokens,
            "wsl-manage-sparse-off",
            "设为非稀疏",
            Rc::new(move |window, cx| sparse_off.update(cx, |page, cx| page.set_sparse(false, window, cx))),
          )),
      )
      .child(
        div()
          .flex()
          .items_end()
          .gap(px(6.))
          .child(
            div()
              .flex_1()
              .min_w(px(0.))
              .child(input_field(tokens, cx, "磁盘大小（如 512GB）", &session.resize)),
          )
          .child(manage_button(
            tokens,
            "wsl-manage-resize",
            "调整",
            Rc::new(move |window, cx| resize.update(cx, |page, cx| page.submit_resize(window, cx))),
          ))
          .flex_none(),
      )
      .child(
        div()
          .flex()
          .items_end()
          .gap(px(6.))
          .child(
            div()
              .flex_1()
              .min_w(px(0.))
              .child(input_field(tokens, cx, "移动到新位置", &session.move_to)),
          )
          .child(manage_button(
            tokens,
            "wsl-manage-move",
            "移动",
            Rc::new(move |window, cx| move_to.update(cx, |page, cx| page.submit_move(window, cx))),
          ))
          .flex_none(),
      );

    // 组间横线分隔：八组条目连排时组边界一眼可辨
    let divider = || zhimaterm_base::ui::horizontal_divider(tokens);
    div()
      .flex()
      .flex_col()
      .gap(px(12.))
      .child(Self::group(tokens, "启动", boot))
      .child(divider())
      .child(Self::group(tokens, "网络", network))
      .child(divider())
      .child(Self::group(tokens, "互操作", interop))
      .child(divider())
      .child(Self::group(tokens, "自动挂载", automount))
      .child(divider())
      .child(Self::group(tokens, "GPU", gpu))
      .child(divider())
      .child(Self::group(tokens, "时间", time))
      .child(divider())
      .child(Self::group(tokens, "用户", user))
      .child(divider())
      .child(Self::group(tokens, "管理操作", manage))
  }
}

/// 新增 WSL 模态（Modal 物种）：镜像下拉（异步加载候选）+ 安装设置表单，
/// 「安装」固定在模态动作区——设置项再多、候选再长都不挤按钮。状态归
/// 页面实体（与其它安装字段同源），模态只是视图；下拉展开的浮层经
/// `floaters` 挂浮层栈根层（不受卡片裁剪，ADR-044）。
struct WslInstallModal {
  page: Entity<WslManagerPage>,
}

impl zhimaterm_base::ui::overlay::ModalView for WslInstallModal {
  fn species(&self) -> zhimaterm_base::ui::overlay::Species {
    zhimaterm_base::ui::overlay::Species::Modal
  }

  fn title(&self) -> Option<String> {
    Some("新增 WSL".into())
  }

  /// 说明区：校验错误（用户名非法/未选分发版）画在动作区上方。
  fn footer(&self, cx: &App) -> Option<AnyElement> {
    let page = self.page.read(cx);
    let notice = page.install_notice.as_ref()?;
    Some(panel::error_line(&theme::tokens(cx), notice).into_any_element())
  }

  /// 表单内容（固定项数）：镜像下拉 + 安装设置，「安装」固定在动作区。
  fn content(&self, _window: &Window, cx: &App) -> AnyElement {
    let tokens = theme::tokens(cx);
    let page_entity = self.page.clone();
    let page = self.page.read(cx);
    let busy = page.busy_idling();
    let optional_input = |tokens: &UiTokens, label: &'static str, input: &Entity<TextInput>| {
      div()
        .flex_1()
        .min_w(px(0.))
        .child(input_field(tokens, cx, label, input))
    };
    // 安装位置：输入框 + 目录选择按钮（选择后回填，仍可手改）。行用
    // items_end 对齐——字段是「标签+输入框」两行块，按钮要贴输入框那一行
    // （与连接表单的私钥选择行同一结构）
    let browse_page = self.page.clone();
    let location = div()
      .flex()
      .items_end()
      .gap(px(6.))
      .child(div().flex_1().min_w(px(0.)).child(input_field(
        &tokens,
        cx,
        "安装位置（留空 = 系统默认）",
        &page.install_location,
      )))
      .child(panel::secondary_button(
        &tokens,
        "wsl-install-browse",
        "选择",
        None,
        move |_, _, cx| browse_page.update(cx, |page, cx| page.pick_install_location(cx)),
      ));
    // 档位选项：勾选框 + 标题 + 下方解释为一组、各占满整行。复用
    // panel::checkbox（base UI 组件），不自造样式
    let option = |tokens: &UiTokens,
                  id: &'static str,
                  label: &'static str,
                  active: bool,
                  flag: InstallFlag,
                  explanation: &'static str,
                  page: &Entity<WslManagerPage>| {
      let page = page.clone();
      panel::checkbox(tokens, id, label, explanation, active, move |_, _, cx| {
        page.update(cx, |page, cx| page.toggle_install_flag(flag, cx))
      })
    };
    // 内边距归 Card 默认滚动容器，这里只排内容本身
    div()
      .flex()
      .flex_col()
      .gap(px(8.))
      .child(page.render_mirror_select(&tokens, &page_entity))
      .child(location)
      .child(optional_input(
        &tokens,
        "自定义名称（留空 = 与 Distribution 同名）",
        &page.install_name,
      ))
      .child(optional_input(
        &tokens,
        "自定义用户名（留空 = Distribution 默认）",
        &page.install_user,
      ))
      .child(optional_input(
        &tokens,
        "磁盘大小（如 256GB，留空 = 默认）",
        &page.install_vhd_size,
      ))
      .child(option(
        &tokens,
        "wsl-install-no-launch",
        "装完不自动启动",
        page.install_no_launch,
        InstallFlag::NoLaunch,
        "装完不自动启动该 Distribution、不进首次设置向导，回到管理页列表手动打开。",
        &self.page,
      ))
      .child(option(
        &tokens,
        "wsl-install-web-download",
        "从网络下载",
        page.install_web_download,
        InstallFlag::WebDownload,
        "从网络源而非 Microsoft Store 下载 Distribution；商店登录或同步有问题时勾选。",
        &self.page,
      ))
      .child(option(
        &tokens,
        "wsl-install-fixed-vhd",
        "固定大小磁盘",
        page.install_fixed_vhd,
        InstallFlag::FixedVhd,
        "创建时按上方「磁盘大小」上限一次性占满文件（性能更稳、占用即刻确定）；不勾则按需增长。默认上限 1TB。",
        &self.page,
      ))
      .child(
        div()
          .flex_none()
          .text_size(ui_rems(10.))
          .text_color(gpui::rgb(tokens.fg_muted))
          .child(if busy {
            "有命令在运行，操作已暂停"
          } else {
            "wsl --install -d；确认后开始下载安装，进度与结果在管理页头部状态行。自定义用户名在装完后创建并设为默认（会短暂启动一次该 Distribution），勾选「装完不自动启动」时随后终止。"
          }),
      )
      .into_any_element()
  }

  fn actions(&self, cx: &App) -> AnyElement {
    let tokens = theme::tokens(cx);
    let page = self.page.read(cx);
    let busy = page.busy_idling();
    let has_mirror = matches!(page.online, Online::Ready(_)) && page.install_selected.is_some();
    let submit = self.page.clone();
    panel::dialog_actions(
      [
        panel::primary_button(&tokens, "wsl-install-run", "安装", None, move |_, window, cx| {
          if !busy && has_mirror {
            submit.update(cx, |page, cx| page.submit_page_install(window, cx))
          }
        })
        .into_any_element(),
      ],
      Some(
        // 关闭 = 关浮层栈顶（与 Esc/遮罩同一语义）；页面安装态在重开时重置。
        // 此前误只翻页面内的下拉标志，浮层不出栈，模态关不掉
        panel::secondary_button(&tokens, "wsl-install-close", "关闭", Some("esc"), |_, window, cx| {
          zhimaterm_base::ui::overlay::overlay_close_top(window, cx);
        })
        .into_any_element(),
      ),
    )
    .into_any_element()
  }

  /// 镜像下拉展开时的候选浮层与全窗口 backdrop：挂浮层栈根层（ADR-044），
  /// 不受卡片 overflow_hidden 裁剪。backdrop 截住一切点击（含卡片），点哪
  /// 里都收起下拉，不让它冒泡到模态遮罩把整个安装模态关掉。
  fn floaters(&self, window: &Window, cx: &App) -> Vec<AnyElement> {
    let tokens = theme::tokens(cx);
    let page = self.page.read(cx);
    if !page.install_open {
      return Vec::new();
    }
    let Online::Ready(names) = &page.online else {
      return Vec::new();
    };
    if names.is_empty() {
      return Vec::new();
    }
    // selected 参数按下标比较标选中行：越界值 = 无选中标记
    let selected = page.install_selected.unwrap_or(usize::MAX);
    let highlighted = page.install_highlight;
    let scroll = page.install_scroll.clone();
    let bounds = page.install_bounds.clone();
    // 按下即拾取：候选是单击语义，不需要字体列表那套按下拖选；按下与
    // 松开都落同一个拾取动作（此前共用一个闭包，press 带上按下事件后分开）
    let press_page = self.page.clone();
    let press = Rc::new(
      move |index: usize, _: &gpui::MouseDownEvent, _: &mut Window, cx: &mut App| {
        press_page.update(cx, |page, cx| page.pick_install_mirror(index, cx));
      },
    );
    let pick_page = self.page.clone();
    let pick = Rc::new(move |index: usize, _: &mut Window, cx: &mut App| {
      pick_page.update(cx, |page, cx| page.pick_install_mirror(index, cx));
    });
    let backdrop = select_backdrop({
      let page = self.page.clone();
      move |_, cx| page.update(cx, |page, cx| page.close_install_dropdown(cx))
    })
    .into_any_element();
    let popover = plain_select_popover(
      &tokens,
      "wsl-mirror-popover",
      names,
      selected,
      highlighted,
      &bounds,
      window.viewport_size().height,
      &scroll,
      press,
      pick,
    );
    vec![backdrop, popover]
  }

  /// 键裁决：下拉展开时 Esc 先收下拉、上下键移动高亮、Enter 拾取；没有
  /// 下拉时回落默认（裸 Esc 关安装模态）。Enter 不绑安装——那是大下载，
  /// 显式点按钮才触发。
  fn key_action(&self, key: &Keystroke, cx: &App) -> KeyHandling {
    if self.page.read(cx).install_open && bare_key(key) {
      let page = self.page.clone();
      return match key.key.as_str() {
        "down" | "up" => {
          let step = if key.key == "down" { 1 } else { -1 };
          KeyHandling::Action(Rc::new(move |_, cx| {
            page.update(cx, |page, cx| page.move_install_highlight(step, cx));
          }))
        }
        "enter" => KeyHandling::Action(Rc::new(move |_, cx| {
          page.update(cx, |page, cx| page.pick_install_highlight(cx));
        })),
        "escape" => KeyHandling::Action(Rc::new(move |_, cx| {
          page.update(cx, |page, cx| page.close_install_dropdown(cx));
        })),
        _ => KeyHandling::None,
      };
    }
    zhimaterm_base::ui::overlay::default_key_action(key)
  }

  fn as_any(&self) -> &dyn std::any::Any {
    self
  }

  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}
