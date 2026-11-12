//! 转发规则草稿（GLOSSARY「转发规则草稿」）：连接表单里的行内规则与
//! 运行中加规则的模态（`forward_form`）共用的编辑草稿——三个窄输入框、
//! 类型与监听地址开关、草稿 → `ForwardRule` 的投影与校验只有这一份。
//! 容器差异（行内错误位 / 模态焦点环）留在各自的表单里。

use gpui::{App, AppContext as _, Entity, Font};

use zhimaterm_base::ui::input::TextInput;
use zhimaterm_session::{ForwardFlow, ForwardId, ForwardRule};

/// 端口文本 → u16。错误文案带字段名给用户看。
pub(crate) fn parse_port(text: &str, field: &str) -> Result<u16, String> {
  let text = text.trim();
  let port: u16 = text
    .parse()
    .map_err(|_| format!("{field}不是 1-65535 的数字：{text}"))?;
  if port == 0 {
    return Err(format!("{field} 不能为 0"));
  }
  Ok(port)
}

/// 转发规则类型（ADR-011）：本地转发 / 远程转发 / SOCKS5。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ForwardKind {
  Local,
  Remote,
  Socks,
}

impl ForwardKind {
  /// 循环切换转发类型（类型 chip 的点击行为）：L → R → D → L。
  pub(crate) fn toggle(self) -> Self {
    match self {
      Self::Local => Self::Remote,
      Self::Remote => Self::Socks,
      Self::Socks => Self::Local,
    }
  }

  /// 类型标签（chip 文案）。
  pub(crate) fn label(self) -> &'static str {
    match self {
      Self::Local => "本地转发",
      Self::Remote => "远程转发",
      Self::Socks => "SOCKS5",
    }
  }

  /// 是否在本地监听（L 与 D 都有绑定地址选项；R 的监听在远端）。
  pub(crate) fn binds_locally(self) -> bool {
    !matches!(self, Self::Remote)
  }

  /// 监听端口的字段名（错误文案用）：本地/SOCKS 听在本机，远程转发的
  /// 端口监听在远端——按类型说，指向性更强。
  fn listen_field(self) -> &'static str {
    match self {
      Self::Local | Self::Socks => "本地端口",
      Self::Remote => "远端端口",
    }
  }
}

/// 输入框的建造参数：(占位文案, 预填文本)。
pub(crate) type DraftInput<'a> = (&'a str, &'a str);

/// 一条转发规则的编辑草稿：三个窄输入框 + 类型与监听地址的开关。
pub(crate) struct ForwardRuleDraft {
  pub(crate) kind: ForwardKind,
  /// 本地监听：是否监听全部网卡（默认只听 127.0.0.1，见 ADR-011）
  pub(crate) all_interfaces: bool,
  pub(crate) listen_port: Entity<TextInput>,
  pub(crate) target_host: Entity<TextInput>,
  pub(crate) target_port: Entity<TextInput>,
  /// 这条规则的校验错误：画在哪（行内盒子 / 模态 footer）由容器决定。
  pub(crate) error: Option<String>,
}

impl ForwardRuleDraft {
  /// 建一条草稿：占位文案与预填值由容器给——行内规则用位置语义当占位
  /// （监听 / 转发到），模态给示例值。关联函数：调用点常正持有草稿列表
  /// 的可变借用。
  pub(crate) fn new(
    font: &Font,
    kind: ForwardKind,
    all_interfaces: bool,
    listen: DraftInput<'_>,
    target_host: DraftInput<'_>,
    target_port: DraftInput<'_>,
    cx: &mut App,
  ) -> Self {
    let make = |(placeholder, text): DraftInput<'_>, cx: &mut App| {
      cx.new(|cx| {
        let mut input = TextInput::new(cx, placeholder, false, font.clone());
        input.set_text(text, cx);
        input
      })
    };
    Self {
      kind,
      all_interfaces,
      listen_port: make(listen, cx),
      target_host: make(target_host, cx),
      target_port: make(target_port, cx),
      error: None,
    }
  }

  /// 三个输入框都是空的：还没动笔的草稿，不预报错。
  pub(crate) fn pristine(&self, cx: &App) -> bool {
    self.listen_port.read(cx).text().trim().is_empty()
      && self.target_host.read(cx).text().trim().is_empty()
      && self.target_port.read(cx).text().trim().is_empty()
  }

  /// 某个输入框实体是否属于这条草稿（编辑回调按实体 id 找回下标用）。
  pub(crate) fn has_input(&self, id: gpui::EntityId) -> bool {
    self.listen_port.entity_id() == id || self.target_host.entity_id() == id || self.target_port.entity_id() == id
  }

  /// 草稿当前值；输入不完整时返回 Err（原因给用户看）。
  pub(crate) fn rule(&self, id: ForwardId, cx: &App) -> Result<ForwardRule, String> {
    let listen = parse_port(self.listen_port.read(cx).text(), self.kind.listen_field())?;
    let bind_addr = if self.all_interfaces { "0.0.0.0" } else { "127.0.0.1" };
    // SOCKS 没有固定目标：目标由每条连接的握手请求决定
    let flow = match self.kind {
      ForwardKind::Socks => ForwardFlow::Socks {
        bind_addr: bind_addr.into(),
        bind_port: listen,
      },
      ForwardKind::Local => {
        let (target_host, target_port) = self.target(cx)?;
        ForwardFlow::Local {
          bind_addr: bind_addr.into(),
          bind_port: listen,
          target_host,
          target_port,
        }
      }
      ForwardKind::Remote => {
        let (target_host, target_port) = self.target(cx)?;
        ForwardFlow::Remote {
          remote_port: listen,
          target_host,
          target_port,
        }
      }
    };
    Ok(ForwardRule { id, flow })
  }

  /// 目标主机与端口（SOCKS 不使用）。
  fn target(&self, cx: &App) -> Result<(String, u16), String> {
    let target_port = parse_port(self.target_port.read(cx).text(), "目标端口")?;
    let target_host = self.target_host.read(cx).text().trim().to_owned();
    if target_host.is_empty() {
      return Err("请填写转发的目标主机".into());
    }
    Ok((target_host, target_port))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn font() -> Font {
    zhimaterm_base::font::terminal_font("monospace")
  }

  fn draft(
    cx: &mut gpui::TestAppContext,
    kind: ForwardKind,
    all_interfaces: bool,
    listen: &str,
    host: &str,
    port: &str,
  ) -> ForwardRuleDraft {
    cx.update(|cx| {
      let font = font();
      ForwardRuleDraft::new(
        &font,
        kind,
        all_interfaces,
        ("端口", listen),
        ("目标主机", host),
        ("端口", port),
        cx,
      )
    })
  }

  #[gpui::test]
  fn parse_port_trims_and_rejects_non_numbers_and_zero(cx: &mut gpui::TestAppContext) {
    let _ = cx;
    assert_eq!(parse_port(" 8080 ", "本地端口"), Ok(8080));
    assert_eq!(
      parse_port("abc", "本地端口"),
      Err("本地端口不是 1-65535 的数字：abc".to_owned())
    );
    assert_eq!(parse_port("0", "本地端口"), Err("本地端口 不能为 0".to_owned()));
    assert_eq!(
      parse_port("65536", "远端端口"),
      Err("远端端口不是 1-65535 的数字：65536".to_owned())
    );
  }

  #[gpui::test]
  fn kind_cycles_labels_and_bind_scope() {
    assert_eq!(ForwardKind::Local.toggle(), ForwardKind::Remote);
    assert_eq!(ForwardKind::Remote.toggle(), ForwardKind::Socks);
    assert_eq!(ForwardKind::Socks.toggle(), ForwardKind::Local);
    assert_eq!(ForwardKind::Socks.label(), "SOCKS5");
    assert!(ForwardKind::Local.binds_locally());
    assert!(ForwardKind::Socks.binds_locally());
    assert!(!ForwardKind::Remote.binds_locally());
  }

  #[gpui::test]
  fn local_draft_projects_into_a_local_rule(cx: &mut gpui::TestAppContext) {
    let draft = draft(cx, ForwardKind::Local, false, "8080", "db.internal", "5432");
    cx.update(|cx| {
      let rule = draft.rule(0, cx).expect("完整输入投影成规则");
      assert_eq!(
        rule.flow,
        ForwardFlow::Local {
          bind_addr: "127.0.0.1".into(),
          bind_port: 8080,
          target_host: "db.internal".to_owned(),
          target_port: 5432,
        }
      );
    });
  }

  #[gpui::test]
  fn all_interfaces_widens_the_bind_address(cx: &mut gpui::TestAppContext) {
    let draft = draft(cx, ForwardKind::Socks, true, "1080", "", "");
    cx.update(|cx| {
      let rule = draft.rule(1, cx).expect("SOCKS 不需要目标");
      assert_eq!(
        rule.flow,
        ForwardFlow::Socks {
          bind_addr: "0.0.0.0".into(),
          bind_port: 1080,
        }
      );
    });
  }

  #[gpui::test]
  fn remote_draft_listens_on_the_remote_port(cx: &mut gpui::TestAppContext) {
    let draft = draft(cx, ForwardKind::Remote, false, "2222", "db.internal", "5432");
    cx.update(|cx| {
      let rule = draft.rule(2, cx).expect("完整输入投影成规则");
      assert_eq!(
        rule.flow,
        ForwardFlow::Remote {
          remote_port: 2222,
          target_host: "db.internal".to_owned(),
          target_port: 5432,
        }
      );
    });
  }

  #[gpui::test]
  fn listen_port_errors_name_the_field_by_kind(cx: &mut gpui::TestAppContext) {
    // 统一口径（连接表单既有文案）：本地/SOCKS 说「本地端口」，远程
    // 转发的端口监听在远端，说「远端端口」——模态旧文案「监听端口」弃用
    let local = draft(cx, ForwardKind::Local, false, "abc", "db.internal", "5432");
    cx.update(|cx| {
      assert_eq!(local.rule(0, cx), Err("本地端口不是 1-65535 的数字：abc".to_owned()));
    });
    let remote = draft(cx, ForwardKind::Remote, false, "0", "db.internal", "5432");
    cx.update(|cx| {
      assert_eq!(remote.rule(0, cx), Err("远端端口 不能为 0".to_owned()));
    });
  }

  #[gpui::test]
  fn target_errors_cover_port_and_host(cx: &mut gpui::TestAppContext) {
    let bad_port = draft(cx, ForwardKind::Local, false, "8080", "db.internal", "abc");
    cx.update(|cx| {
      assert_eq!(bad_port.rule(0, cx), Err("目标端口不是 1-65535 的数字：abc".to_owned()));
    });
    let no_host = draft(cx, ForwardKind::Local, false, "8080", "  ", "5432");
    cx.update(|cx| {
      assert_eq!(no_host.rule(0, cx), Err("请填写转发的目标主机".to_owned()));
    });
    let socks = draft(cx, ForwardKind::Socks, false, "1080", "ignored", "ignored");
    cx.update(|cx| {
      assert!(socks.rule(0, cx).is_ok(), "SOCKS 的目标框不参与校验");
    });
  }

  #[gpui::test]
  fn pristine_means_untouched(cx: &mut gpui::TestAppContext) {
    let empty = draft(cx, ForwardKind::Local, false, "", "", "");
    cx.update(|cx| {
      assert!(empty.pristine(cx));
      assert!(empty.has_input(empty.listen_port.entity_id()));
      assert!(!empty.has_input(gpui::EntityId::from(0u64)));
    });
    let touched = draft(cx, ForwardKind::Local, false, "8080", "", "");
    cx.update(|cx| {
      assert!(!touched.pristine(cx));
    });
  }
}
