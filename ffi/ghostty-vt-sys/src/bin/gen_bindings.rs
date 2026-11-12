//! 从 pin 的 Ghostty 头文件再生成 `src/bindings.rs`。
//!
//! `cargo run -p ghostty-vt-sys --features bindgen-tool --bin gen-bindings`。
//! 头文件解析顺序：`GHOSTTY_INCLUDE_DIR` → `GHOSTTY_SOURCE_DIR/include` →
//! workspace 构建缓存（`target/*/build/ghostty-vt-sys-*/out/src/include`，
//! 即 build.rs 下载解包的 pin 源码树）。步骤详见
//! docs/runbooks/ghostty-bindings.md。
//!
//! 改编自 libghostty-vt-sys 0.2.2（MIT OR Apache-2.0）：上游对全部
//! `ghostty_*` 符号生成绑定，这里改为精确 allowlist——只生成后端用到的
//! 函数与类型，C API 未用到的族（选择/搜索/键编码/快照…）不进本仓。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use bindgen::EnumVariation;
use bindgen::callbacks::{EnumVariantValue, IntKind, ItemInfo, ItemKind, ParseCallbacks};

use heck::ToShoutySnakeCase;

/// 后端用到的 C 函数。tagged get/set（`ghostty_terminal_get/set` 等）的
/// 载荷类型不出现在任何函数签名里，必须配合下面的 TYPES 显式放行。
const FUNCTIONS: &[&str] = &[
  // 生命周期 / 输入 / resize / 滚动
  "ghostty_terminal_new",
  "ghostty_terminal_free",
  "ghostty_terminal_vt_write",
  "ghostty_terminal_resize",
  "ghostty_terminal_scroll_viewport",
  // tagged 属性读写（模式、颜色、调色板、回调安装……）
  "ghostty_terminal_get",
  "ghostty_terminal_set",
  // 网格寻址（回看慢路径）
  "ghostty_terminal_grid_ref",
  "ghostty_grid_ref_cell",
  "ghostty_grid_ref_row",
  "ghostty_grid_ref_style",
  "ghostty_grid_ref_graphemes",
  "ghostty_grid_ref_hyperlink_uri",
  // 视口渲染快照
  "ghostty_render_state_new",
  "ghostty_render_state_free",
  "ghostty_render_state_update",
  "ghostty_render_state_get",
  "ghostty_render_state_row_iterator_new",
  "ghostty_render_state_row_iterator_free",
  "ghostty_render_state_row_iterator_next",
  "ghostty_render_state_row_get",
  "ghostty_render_state_row_cells_new",
  "ghostty_render_state_row_cells_free",
  "ghostty_render_state_row_cells_select",
  "ghostty_render_state_row_cells_get",
  // cell/row/style 低层 tagged get
  "ghostty_cell_get",
  "ghostty_row_get",
];

/// 直接按名引用的类型：不透明句柄、回调函数指针、tagged get/set 的载荷
/// 结构体与按值传递的 tagged union。allowlist 按 C 原名匹配（`Ghostty`
/// 前缀），`item_name` 回调只改输出名。
const TYPES: &[&str] = &[
  // 不透明句柄
  "GhosttyTerminalImpl",
  "GhosttyRenderStateImpl",
  "GhosttyRenderStateRowIteratorImpl",
  "GhosttyRenderStateRowCellsImpl",
  // 回调（构造时经 terminal_set 安装；pwd/通知/进度的回调不装——
  // 适配层不消费这些事件，vt.h 语义是未装即关闭该效果）
  "GhosttyTerminalWritePtyFn",
  "GhosttyTerminalBellFn",
  "GhosttyTerminalTitleChangedFn",
  "GhosttyTerminalEnquiryFn",
  "GhosttyTerminalXtversionFn",
  "GhosttyTerminalSizeFn",
  "GhosttyTerminalColorSchemeFn",
  "GhosttyTerminalDeviceAttributesFn",
  "GhosttyTerminalClipboardWriteFn",
  "GhosttyTerminalRenderHoldFn",
  // 回调载荷（桌面通知/进度上报的回调未安装：适配层不消费，载荷不需要）
  "GhosttySizeReportSize",
  "GhosttyColorScheme",
  "GhosttyDeviceAttributes",
  "GhosttyDeviceAttributesPrimary",
  "GhosttyClipboardWrite",
  "GhosttyClipboardContent",
  "GhosttyClipboardWriteReply",
  "GhosttyClipboardWriteResult",
  // terminal tagged get/set 的载荷
  "GhosttyTerminalOption",
  "GhosttyTerminalData",
  "GhosttyTerminalModeConfig",
  "GhosttyMode",
  "GhosttyResult",
  "GhosttyTerminalScrollbar",
  "GhosttyTerminalScrollViewport",
  "GhosttyColorRgb",
  "GhosttyString",
  "GhosttyBuffer",
  "GhosttyGridRef",
  "GhosttyPoint",
  "GhosttyStyle",
  "GhosttyStyleColor",
  // 1.0 起 GhosttyStyleColor.tag 是裸 u8，枚举不再是字段依赖，显式放行
  "GhosttyStyleColorTag",
  "GhosttySgrUnderline",
  // render state 路径
  "GhosttyRenderStateData",
  "GhosttyRenderStateRowData",
  "GhosttyRenderStateRowCellsData",
  "GhosttyCursorStyle",
  "GhosttyRenderStateCursor",
  "GhosttyCell",
  "GhosttyCellData",
  "GhosttyCellWide",
  "GhosttyRow",
  "GhosttyRowData",
];

/// 直接引用的头文件宏常量（bindgen var）：DA 应答的 VT 级别/机型、
/// kitty keyboard 协议位、STYLE_UNDERLINE_SHIFT。GhosttyStyle 的 flags
/// 位（1.0 起样式属性打包进位掩码）里只有 SHIFT 是纯字面量宏，能生成；
/// FLAG_* / MASK / UNDERLINE 都是 cast 或函数式宏，bindgen 一律不生成，
/// 位值在适配层 `vt.rs` 的 `style_flag` 按头文件位序手写。
const VARS: &[&str] = &["GHOSTTY_DA_.*", "GHOSTTY_KITTY_KEY_.*", "GHOSTTY_STYLE_UNDERLINE_SHIFT"];

fn main() {
  let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"));

  let include_dir = resolve_include_dir(&manifest_dir);
  let header = include_dir.join("ghostty").join("vt.h");
  let out = manifest_dir.join("src").join("bindings.rs");

  let mut builder = bindgen::Builder::default()
    .header(header.to_string_lossy())
    .clang_arg(format!("-I{}", include_dir.to_string_lossy()))
    // 链接的是静态库，头文件不能带 dllimport/可见性属性。
    .clang_arg("-DGHOSTTY_STATIC")
    .generate_cstr(true)
    .derive_default(true)
    .size_t_is_usize(true)
    .default_enum_style(EnumVariation::ModuleConsts)
    .parse_callbacks(Box::new(Callbacks));
  for function in FUNCTIONS {
    builder = builder.allowlist_function(function);
  }
  for ty in TYPES {
    builder = builder.allowlist_type(ty);
  }
  for var in VARS {
    builder = builder.allowlist_var(var);
  }

  let bindings = builder
    .generate()
    .expect("failed to generate bindings from include/ghostty/vt.h");

  bindings
    .write_to_file(&out)
    .unwrap_or_else(|error| panic!("failed to write bindings to {}: {error}", out.display()));
}

/// 头文件目录：环境变量优先，其次复用 build.rs 下载解包的 pin 源码树。
fn resolve_include_dir(manifest_dir: &Path) -> PathBuf {
  if let Ok(dir) = env::var("GHOSTTY_INCLUDE_DIR") {
    return PathBuf::from(dir);
  }
  if let Ok(src) = env::var("GHOSTTY_SOURCE_DIR") {
    return PathBuf::from(src).join("include");
  }
  let target = manifest_dir.join("..").join("..").join("target");
  for profile in ["debug", "release"] {
    let builds = target.join(profile).join("build");
    let Ok(entries) = fs::read_dir(&builds) else {
      continue;
    };
    let mut found: Vec<PathBuf> = entries
      .flatten()
      .map(|entry| entry.path().join("out").join("src").join("include"))
      .filter(|dir| dir.join("ghostty").join("vt.h").exists())
      .collect();
    if let Some(dir) = found.pop() {
      return dir;
    }
  }
  panic!(
    "could not find ghostty headers; build once so build.rs downloads the pinned \
         source, or set GHOSTTY_INCLUDE_DIR or GHOSTTY_SOURCE_DIR (see \
         docs/runbooks/ghostty-bindings.md)"
  );
}

/// 去掉 C 前缀后仍不唯一的枚举变体前缀。
const PREFIXES: &[(&str, &str)] = &[
  ("GhosttyTerminalOption", "GHOSTTY_TERMINAL_OPT"),
  ("GhosttyTerminalScrollViewportTag", "GHOSTTY_TERMINAL_SCROLL_VIEWPORT"),
  ("GhosttyStyleColorTag", "GHOSTTY_STYLE_COLOR"),
  ("GhosttyResult", "GHOSTTY_"),
];

#[derive(Debug)]
struct Callbacks;

impl ParseCallbacks for Callbacks {
  fn item_name(&self, item_info: ItemInfo) -> Option<String> {
    let prefix = match item_info.kind {
      // 函数不改名：bindgen 会给 link_name 加 `\u{1}` 前缀，Apple
      // 链接器不支持（rust-lang/rust-bindgen#1221）。
      ItemKind::Function => return None,
      ItemKind::Var => "GHOSTTY_",
      _ => "Ghostty",
    };
    Some(item_info.name.trim_start_matches(prefix).to_string())
  }

  fn enum_variant_name(
    &self,
    enum_name: Option<&str>,
    original_variant_name: &str,
    _variant_value: EnumVariantValue,
  ) -> Option<String> {
    let enum_name = enum_name?;

    // 去掉 C 枚举变体里的冗余前缀
    let prefix = PREFIXES
      .iter()
      .find(|(v, _)| *v == enum_name)
      .map(|(_, n)| n.to_string())
      .unwrap_or_else(|| enum_name.to_shouty_snake_case());

    let transformed = original_variant_name
      .trim_start_matches(&prefix)
      .trim_start_matches('_');

    Some(transformed.to_string())
  }

  fn process_comment(&self, comment: &str) -> Option<String> {
    Some(
      comment
        .lines()
        // doxygen 指令不是文档内容
        .filter(|s| !s.trim().starts_with("@"))
        .collect::<Vec<_>>()
        .join("\n"),
    )
  }

  fn int_macro(&self, name: &str, _value: i64) -> Option<IntKind> {
    // 少数宏的类型修正，减少调用侧手写 cast
    if name.starts_with("GHOSTTY_DA_") || name.starts_with("GHOSTTY_MODS_") {
      Some(IntKind::U16)
    } else if name.starts_with("GHOSTTY_KITTY_KEY_") {
      Some(IntKind::U8)
    } else {
      None
    }
  }
}
