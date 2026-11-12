//! libghostty-vt 的原始 FFI 绑定（ADR-051）。
//!
//! 绑定对官方头文件 `include/ghostty/vt.h` 预生成后提交：源码按 GHOSTTY.lock
//! 的 sha256 pin，绑定天然稳定，日常构建只需要 Zig，不需要 bindgen/libclang。
//! 再生成与更新 pin 的步骤见 docs/runbooks/ghostty-bindings.md。
//!
//! 只导出裸绑定与 ABI 协议层的最小帮手（sized 结构、借用字符串）；
//! 语义封装在后端适配层（src/terminal/backend/ghostty/）。

/// bindgen 生成物：只含后端用到的函数（allowlist）及其传递依赖的类型。
// 生成代码不复刻本仓命名规范，lint 关在模块内，不渗到别处。
#[allow(clippy::all)]
#[allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]
#[allow(dead_code, nonstandard_style, clippy::undocumented_unsafe_blocks)]
pub mod bindings;

pub use bindings::*;

use std::ops::Deref;

/// 初始化「sized」FFI 结构：首字段 size 是 ABI 版本协商位，必须写当前大小，
/// C 侧只触达调用方已知的字段。
#[macro_export]
macro_rules! sized {
  ($ty:ty) => {{
    let mut t = <$ty as ::std::default::Default>::default();
    t.size = ::std::mem::size_of::<$ty>();
    t
  }};
}

impl<S> From<S> for bindings::String
where
  S: Deref<Target = str>,
{
  fn from(value: S) -> Self {
    Self {
      ptr: value.as_ptr(),
      len: value.len(),
    }
  }
}

impl PartialEq for bindings::ColorRgb {
  fn eq(&self, other: &Self) -> bool {
    self.r == other.r && self.g == other.g && self.b == other.b
  }
}

impl Eq for bindings::ColorRgb {}

impl bindings::String {
  /// 借用为原始字节。空串的指针 C 侧不保证非空，而
  /// `slice::from_raw_parts` 零长度也要求非空指针，null 归一成空切片。
  ///
  /// # Safety
  ///
  /// 调用方保证关联生命周期内在 FFI 上下文里有效。
  pub unsafe fn to_bytes<'a>(self) -> &'a [u8] {
    if self.ptr.is_null() {
      return &[];
    }
    // SAFETY: 由调用方保证
    unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
  }

  /// # Safety
  ///
  /// 同 [`String::to_bytes`]，且内容是合法 UTF-8。
  pub unsafe fn to_str<'a>(self) -> &'a str {
    // SAFETY: 由调用方保证
    unsafe { std::str::from_utf8_unchecked(self.to_bytes()) }
  }
}
