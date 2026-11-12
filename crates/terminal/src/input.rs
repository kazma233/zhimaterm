//! 输入编码：按键、粘贴、焦点与鼠标手势 → 终端字节（ADR-007 修订）。
//!
//! 编码需要终端模式（DECCKM、kitty disambiguate、鼠标私有模式、括号粘贴、
//! 备用屏滚轮），模式只有 `TerminalModel` 持有，因此方法是 `TerminalModel`
//! 的固有实现、落在输入编码这条变更轴自己的文件里（`transcript`/`selection`
//! 同型先例）。对外的归属裁决类型 `MouseDispatch` 留在 lib.rs（共享语义）。

use zhimaterm_base::input::{KeyInput, MouseAction, MouseButton, MouseGesture, WheelDirection};

use super::TerminalModel;

/// 程序要求的鼠标上报协议：xterm 私有模式的 1000 / 1002 / 1003。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseProtocol {
  ClickOnly,
  Drag,
  Motion,
}

/// xterm 鼠标编码里的按键编号。
const RELEASE_CODE: u32 = 3;
/// 拖动/移动在按键编号上叠加的偏移。
const MOTION_OFFSET: u32 = 32;

fn button_code(button: MouseButton) -> u32 {
  match button {
    MouseButton::Left => 0,
    MouseButton::Middle => 1,
    MouseButton::Right => 2,
  }
}

fn wheel_code(direction: WheelDirection) -> u32 {
  match direction {
    WheelDirection::Up => 64,
    WheelDirection::Down => 65,
    WheelDirection::Left => 66,
    WheelDirection::Right => 67,
  }
}

fn modifier_bits(modifiers: zhimaterm_base::input::GestureModifiers) -> u32 {
  u32::from(modifiers.shift) * 4 + u32::from(modifiers.alt) * 8 + u32::from(modifiers.control) * 16
}

fn push_codepoint(bytes: &mut Vec<u8>, value: u32) {
  let mut buffer = [0u8; 4];
  if let Some(character) = char::from_u32(value) {
    bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
  }
}

/// 光标键与 Home/End 的两种形式：应用光标模式用 SS3（`ESC O x`），
/// 普通模式用 CSI（`ESC [ x`）。模式由前台程序通过 DECCKM 切换。
fn navigation(final_byte: u8, application_cursor: bool, modifier: Option<u8>) -> Vec<u8> {
  match modifier {
    // SS3 无法表达修饰键，带修饰时统一用 CSI 参数形式（xterm/kitty 通行做法）
    Some(modifier) => format!("\x1b[1;{modifier}{}", final_byte as char).into_bytes(),
    None if application_cursor => vec![0x1b, b'O', final_byte],
    None => vec![0x1b, b'[', final_byte],
  }
}

/// 数字终结符的功能键（delete/pageup 等）：修饰参数以 `CSI n;m ~` 并入。
fn tilde(final_byte: u8, modifier: Option<u8>) -> Vec<u8> {
  match modifier {
    Some(modifier) => format!("\x1b[{final_byte};{modifier}~").into_bytes(),
    None => format!("\x1b[{final_byte}~").into_bytes(),
  }
}

/// PF 键（F1–F4）：xterm 无修饰固定发 SS3，不随 DECCKM 变化；带修饰与方向键
/// 同走 `CSI 1;m X`。
fn pf_key(final_byte: u8, modifier: Option<u8>) -> Vec<u8> {
  navigation(final_byte, true, modifier)
}

/// 带独立编码的功能键：修饰键并入参数而不是叠 ESC 前缀，kitty disambiguate 下
/// 也保持 legacy 形式（协议把这些形态列为合法）。
fn is_functional_key(key: &str) -> bool {
  matches!(
    key,
    "up"
      | "down"
      | "right"
      | "left"
      | "home"
      | "end"
      | "insert"
      | "delete"
      | "pageup"
      | "pagedown"
      | "f1"
      | "f2"
      | "f3"
      | "f4"
      | "f5"
      | "f6"
      | "f7"
      | "f8"
      | "f9"
      | "f10"
      | "f11"
      | "f12"
  )
}

/// xterm/kitty 修饰参数：1 + shift + 2·alt + 4·control。
fn modifier_param(key: &KeyInput) -> u8 {
  1 + u8::from(key.shift) + 2 * u8::from(key.alt) + 4 * u8::from(key.control)
}

/// kitty `CSI codepoint ; modifier u`。无修饰时省略修饰参数，与 kitty 实机输出
/// 一致（孤立 Esc 是 `\x1b[27u`）。
fn csi_u(codepoint: u32, modifier: u8) -> Vec<u8> {
  match modifier {
    1 => format!("\x1b[{codepoint}u").into_bytes(),
    modifier => format!("\x1b[{codepoint};{modifier}u").into_bytes(),
  }
}

/// 文本键的 kitty 键码：单字符键名取其码点（字母取小写——键码是未按 shift 的
/// 字符，shift 在修饰参数里），多字符键名（功能键）不是文本键。
fn text_codepoint(key: &str) -> Option<u32> {
  let mut chars = key.chars();
  let first = chars.next()?;
  chars.next().is_none().then(|| first.to_ascii_lowercase() as u32)
}

/// 纯 alt 功能键的 meta 等价物：主流 shell 的默认键表只给这些组合绑了
/// meta 形态（zsh/bash/fish 的 `^[b`/`^[f` 按词移动），修饰 CSI 会被行
/// 编辑器当字面插入。表里没有就是不翻——其余功能键的 meta 形态是双 ESC
/// 废序列，无任何主流绑定。文本键不查表：通用规则本来就给它们发 ESC
/// 前缀（alt+backspace 的 `\x1b\x7f` 即 zsh 默认的 backward-kill-word）。
fn alt_meta_equivalent(key: &str) -> Option<u8> {
  match key {
    "left" => Some(b'b'),
    "right" => Some(b'f'),
    _ => None,
  }
}

impl TerminalModel {
  /// 把剪贴板文本编码为终端输入。bracketed paste 由前台程序通过私有模式
  /// 2004 开关，开启时用 CSI 200~ / 201~ 包裹，并剔除 ESC，
  /// 避免粘贴内容伪造控制序列影响程序状态。
  pub fn encode_paste(&self, text: &str) -> Vec<u8> {
    if self.core.modes().bracketed_paste {
      let sanitized = text.replace('\x1b', "");
      let mut bytes = Vec::with_capacity(sanitized.len() + 12);
      bytes.extend_from_slice(b"\x1b[200~");
      bytes.extend_from_slice(sanitized.as_bytes());
      bytes.extend_from_slice(b"\x1b[201~");
      bytes
    } else {
      text.replace('\r', "").into_bytes()
    }
  }

  /// 按键 → 终端字节。需要终端模式的部分在这里决定：方向键与 Home/End 在
  /// 应用光标模式（DECCKM）下是 SS3 形式，程序（vim/less 等）按 SS3 解析；
  /// 功能键带修饰键时并入 xterm 修饰参数（`\x1b[1;5D`），shell 与 TUI 靠它
  /// 实现 ctrl/alt+方向键按词移动；程序协商 kitty disambiguate（CSI > 1u）
  /// 后，legacy 下有歧义的按键改报 CSI u（`\x1b[27u`、`\x1b[13;5u`、
  /// `\x1b[99;5u`），否则 Esc 与转义序列开头、ctrl+enter 与 enter 都无法区分。
  /// 返回 None 表示这个按键不产生终端输入。
  pub fn encode_key(&self, key: &KeyInput) -> Option<Vec<u8>> {
    let mode = self.core.modes();
    let application_cursor = mode.app_cursor;

    // kitty disambiguate 模式：legacy 编码与别的字节撞车的按键改报 CSI u——
    // Esc（孤立的 `\x1b` 与转义序列开头无法区分，带不带修饰都报）、带 ctrl/alt
    // 的文本键（控制字节与 ESC 前缀都丢信息）、带修饰的 enter/tab/backspace。
    // 无修饰的 enter/tab/backspace 是协议唯一豁免，仍发 legacy 字节：程序崩溃
    // 没来得及 pop 时 shell 里还能敲 reset；shift-only 的文本键本身就是文本，
    // 也走 legacy。这是本仓固化的编码口径（与 kitty 协议规格一致）。
    if mode.disambiguate_esc_codes {
      let modified = key.control || key.alt || key.shift;
      let ctrl_or_alt = key.control || key.alt;
      let codepoint = match key.key.as_str() {
        "escape" => Some(27),
        "enter" if modified => Some(13),
        "tab" if modified => Some(9),
        "backspace" if modified => Some(127),
        "space" if ctrl_or_alt => Some(32),
        // 键码取 `key`（按键身份，不含 option 修饰）：kitty 协议报的是
        // 按键本身，⌥+b 的键码是 'b' = 98，不是 macOS 合成的 ∫。
        _ if ctrl_or_alt => text_codepoint(&key.key),
        _ => None,
      };
      if let Some(codepoint) = codepoint {
        return Some(csi_u(codepoint, modifier_param(key)));
      }
    }

    // 纯 alt 键的 meta 形态——与文本键「Alt 按 Meta 发送」同一条规则：
    // legacy 下若存在全 shell 默认绑定的 meta 等价物（alt_meta_equivalent）
    // 就发它。ctrl/shift 组合与 kitty disambiguate 除外：meta 表达不了
    // ctrl；协商了键盘协议的程序要标准形式（ADR-031 修订 2026-11-10，
    // 排障与各家口径见 docs/context/alt-arrow-word-motion.md）
    if !mode.disambiguate_esc_codes
      && key.alt
      && !key.control
      && !key.shift
      && let Some(meta) = alt_meta_equivalent(&key.key)
    {
      return Some(vec![0x1b, meta]);
    }

    // shift+tab 是 backtab（`CSI Z`）：与功能键同列，ctrl/alt 并入修饰参数而不叠
    // ESC 前缀；shift 本身已由 Z 表达，只有 shift 时不报参数（xterm 口径）。
    let backtab = key.key == "tab" && key.shift;
    let functional = is_functional_key(&key.key) || backtab;
    let modified = key.control || key.alt || key.shift;
    let modifier = (functional && modified).then(|| modifier_param(key));

    // option 修饰的文本键：`key_char` 与基础键不同，说明系统合成了特殊字符
    // （macOS 的 ⌥+b = ∫；Linux/Windows 不会，`key_char` 就是原始字符）。
    // ⌥ 当 Alt 时换成基础键码——程序收到的是「带 alt 的 b」而不是一个 ∫；
    // 不当 Alt 时特殊字符本身就是完整输入，直接发它。
    // 空格是唯一键名（"space"）不等于其字符的文本键，比较与回退都用字符本身，
    // 否则 ⌥+space 会把键名当文本发出去。
    let base_key = if key.key == "space" { " " } else { key.key.as_str() };
    let composed_char = key.key_char.as_deref().is_some_and(|key_char| key_char != base_key);
    let alt_text = if key.alt && !functional && self.option_as_alt && composed_char {
      base_key
    } else {
      key.key_char.as_deref().unwrap_or(base_key)
    };

    let mut bytes = match key.key.as_str() {
      "enter" => b"\r".to_vec(),
      "tab" if backtab => navigation(b'Z', false, modifier.filter(|_| key.control || key.alt)),
      "tab" => b"\t".to_vec(),
      "backspace" => vec![0x7f],
      "escape" => vec![0x1b],
      // 控制字节 0x20 & 0x1f = NUL；键名是 "space" 而非单字符，进不了下面的
      // 单字符 ctrl 分支。
      "space" if key.control => vec![0x00],
      "up" => navigation(b'A', application_cursor, modifier),
      "down" => navigation(b'B', application_cursor, modifier),
      "right" => navigation(b'C', application_cursor, modifier),
      "left" => navigation(b'D', application_cursor, modifier),
      "home" => navigation(b'H', application_cursor, modifier),
      "end" => navigation(b'F', application_cursor, modifier),
      "insert" => tilde(2, modifier),
      "delete" => tilde(3, modifier),
      "pageup" => tilde(5, modifier),
      "pagedown" => tilde(6, modifier),
      "f1" => pf_key(b'P', modifier),
      "f2" => pf_key(b'Q', modifier),
      "f3" => pf_key(b'R', modifier),
      "f4" => pf_key(b'S', modifier),
      "f5" => tilde(15, modifier),
      "f6" => tilde(17, modifier),
      "f7" => tilde(18, modifier),
      "f8" => tilde(19, modifier),
      "f9" => tilde(20, modifier),
      "f10" => tilde(21, modifier),
      "f11" => tilde(23, modifier),
      "f12" => tilde(24, modifier),
      _ if key.control && key.key.chars().count() == 1 => {
        let character = key.key.as_bytes()[0].to_ascii_uppercase();
        if character.is_ascii_alphabetic() || (b'@'..=b'_').contains(&character) {
          vec![character & 0x1f]
        } else {
          return None;
        }
      }
      _ if !key.control => alt_text.as_bytes().to_vec(),
      _ => return None,
    };

    // Alt 按 Meta 发送：ESC 前缀。功能键的 Alt 已并入修饰参数，
    // CSI 序列再叠 ESC 前缀会变成程序不认识的双 ESC。
    // 系统合成的特殊字符（⌥ 不当 Alt 时）本身就是完整输入，再叠 ESC 前缀
    // 会变成「meta + 一个符号」的混合语义，程序无从解码——zsh 会把
    // ESC + ∫ 显示成 <ffffffff>。此时不加前缀。
    if key.alt && !functional && (self.option_as_alt || !composed_char) {
      bytes.insert(0, 0x1b);
    }
    Some(bytes)
  }

  /// 焦点进入/离开。只有前台程序打开 FOCUS_IN_OUT（私有模式 1004）时才上报。
  pub fn encode_focus(&self, focused: bool) -> Option<Vec<u8>> {
    self.core.modes().focus_in_out.then(|| {
      if focused {
        b"\x1b[I".to_vec()
      } else {
        b"\x1b[O".to_vec()
      }
    })
  }

  /// 鼠标手势 → 上报字节。程序没打开鼠标上报、或该动作在这种协议下不上报时返回 None，
  /// 调用方应把这种情况的手势当作本地手势（选区或滚动）处理。
  /// `pub(super)`：单测直调各编码形态；生产路径经 [`Self::dispatch_mouse`]。
  pub(super) fn encode_mouse(&self, gesture: &MouseGesture) -> Option<Vec<u8>> {
    let protocol = self.mouse_protocol()?;
    let mode = self.core.modes();
    let sgr = mode.sgr_mouse;

    let code = match gesture.action {
      MouseAction::Press { button, .. } => button_code(button),
      // SGR 用终止字符区分松开，传统编码固定用 3
      MouseAction::Release(button) => {
        if sgr {
          button_code(button)
        } else {
          RELEASE_CODE
        }
      }
      MouseAction::Motion(Some(button)) => {
        if matches!(protocol, MouseProtocol::Drag | MouseProtocol::Motion) {
          button_code(button) + MOTION_OFFSET
        } else {
          return None;
        }
      }
      MouseAction::Motion(None) => {
        if matches!(protocol, MouseProtocol::Motion) {
          RELEASE_CODE + MOTION_OFFSET
        } else {
          return None;
        }
      }
      MouseAction::Wheel(direction, count) => {
        let code = wheel_code(direction) | modifier_bits(gesture.modifiers);
        // 传统与 SGR 都是「一格一次上报」，滚轮几行就发几份
        return Some(self.encode_mouse_reports(code, gesture, false, count.max(1) as usize));
      }
    };

    let code = code | modifier_bits(gesture.modifiers);
    let release = matches!(gesture.action, MouseAction::Release(_));
    Some(self.encode_mouse_reports(code, gesture, release, 1))
  }

  /// 一次手势重复 `count` 份（滚轮），两种编码都按 1-based 坐标发送。
  fn encode_mouse_reports(&self, code: u32, gesture: &MouseGesture, release: bool, count: usize) -> Vec<u8> {
    let mode = self.core.modes();
    let column = gesture.column as u32 + 1;
    let row = gesture.row as u32 + 1;
    let mut bytes = Vec::with_capacity(16 * count);

    for _ in 0..count {
      if mode.sgr_mouse {
        let terminator = if release { 'm' } else { 'M' };
        bytes.extend_from_slice(format!("\x1b[<{code};{column};{row}{terminator}").as_bytes());
      } else {
        // xterm 的按键编号与坐标都要加 32 偏移，否则程序会解析成按键 0
        bytes.extend_from_slice(b"\x1b[M");
        bytes.push((code + 32) as u8);
        if mode.utf8_mouse {
          // 1005：三个字段都是 Unicode 码点，超过 127 时要按 UTF-8 多字节发送
          push_codepoint(&mut bytes, column + 32);
          push_codepoint(&mut bytes, row + 32);
        } else {
          // 传统编码每项只有一个字节，坐标大于 223 时无法表达，只能截顶
          bytes.push((column + 32).min(255) as u8);
          bytes.push((row + 32).min(255) as u8);
        }
      }
    }

    bytes
  }

  /// 程序要求的鼠标上报协议；None 表示手势由本地处理（选区或滚动）。
  fn mouse_protocol(&self) -> Option<MouseProtocol> {
    let mode = self.core.modes();
    if mode.mouse_motion {
      Some(MouseProtocol::Motion)
    } else if mode.mouse_drag {
      Some(MouseProtocol::Drag)
    } else if mode.mouse_report_click {
      Some(MouseProtocol::ClickOnly)
    } else {
      None
    }
  }

  /// 程序是否接管了鼠标。接管后按下/拖动/滚轮都应上报，而不是变成选区。
  pub fn wants_mouse(&self) -> bool {
    self.mouse_protocol().is_some()
  }

  /// 鼠标手势的归属裁决：程序接管鼠标时一律上报——该手势在当前协议下无需
  /// 上报时字节为空，也不落回本地选区（避免和程序自己的高亮打架）；否则
  /// 备用屏上的纵向滚轮转方向键（1007）；其余归本地选区与滚动历史。
  /// 顺序是终端语义，不是调用方的选择。
  pub fn dispatch_mouse(&self, gesture: &MouseGesture) -> super::MouseDispatch {
    if self.wants_mouse() {
      return super::MouseDispatch::Program(self.encode_mouse(gesture).unwrap_or_default());
    }
    if let Some(bytes) = self.encode_alternate_scroll(gesture) {
      return super::MouseDispatch::Program(bytes);
    }
    super::MouseDispatch::Local
  }

  /// 备用屏滚轮（xterm alternate scroll，私有模式 1007）：程序没开鼠标上报、又在
  /// 备用屏上时，纵向滚轮每格转成一次方向键，形式随 DECCKM——备用屏没有滚动
  /// 历史可翻，less/man/vim 靠这个翻页。主屏、横向滚轮、程序关掉 1007 都返回
  /// None 交给本地处理。只经 [`TerminalModel::dispatch_mouse`] 调用：程序接管
  /// 鼠标时轮不到这里。
  fn encode_alternate_scroll(&self, gesture: &MouseGesture) -> Option<Vec<u8>> {
    let MouseAction::Wheel(direction, count) = gesture.action else {
      return None;
    };
    let mode = self.core.modes();
    if !mode.alt_screen || !mode.alternate_scroll {
      return None;
    }
    let final_byte = match direction {
      WheelDirection::Up => b'A',
      WheelDirection::Down => b'B',
      WheelDirection::Left | WheelDirection::Right => return None,
    };
    let key = navigation(final_byte, mode.app_cursor, None);
    Some(key.repeat(count.max(1) as usize))
  }
}
