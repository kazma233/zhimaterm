//! 已保存的连接 → 会话目标（列表直连路径，ADR-048，不经表单）。

use crate::connections::{SavedConnection, SavedForward};
use zhimaterm_session::{ForwardRule, SshAuth, SshJump, SshTarget};

use super::parse::expand_home;
use super::{Field, FormError};

/// 已保存的连接 → 会话目标（列表上直接连的路径，ADR-048，不经表单）。
///
/// 口令只有调用方交进来的 `password`（钥匙串回填或刚输入的）；私钥口令与
/// 跳板口令没有落盘来源，跳板只靠 agent/私钥。素材不够时报错（落在对应的口令
/// 字段上），由调用方决定要口令还是退到编辑视图。
pub fn target_of_saved(saved: &SavedConnection, password: Option<String>) -> Result<SshTarget, FormError> {
  let auth = SshAuth {
    use_agent: saved.credentials.use_agent,
    key_path: saved
      .credentials
      .key_path
      .as_deref()
      .map(expand_home)
      .unwrap_or_default(),
    key_passphrase: None,
    password: password.filter(|password| !password.is_empty()),
  };
  if auth.is_empty() {
    return Err(FormError::at(Field::Auth, "这条连接需要密码"));
  }
  let forwards = saved
    .forwards
    .iter()
    .enumerate()
    .map(|(index, saved)| ForwardRule {
      id: index as zhimaterm_session::ForwardId,
      flow: flow_of_saved(saved),
    })
    .collect();
  let jump = saved
    .jump
    .as_ref()
    .map(|jump| -> Result<Box<SshJump>, FormError> {
      let auth = SshAuth {
        use_agent: jump.credentials.use_agent,
        key_path: jump
          .credentials
          .key_path
          .as_deref()
          .map(expand_home)
          .unwrap_or_default(),
        key_passphrase: None,
        password: None,
      };
      if auth.is_empty() {
        return Err(FormError::at(
          Field::JumpPassword,
          "跳板机只有密码认证：填好跳板密码后用「保存并连接」",
        ));
      }
      Ok(Box::new(SshJump {
        host: jump.host.clone(),
        port: jump.port,
        user: jump.user.clone(),
        auth,
      }))
    })
    .transpose()?;
  Ok(SshTarget {
    host: saved.host.clone(),
    port: saved.port,
    user: saved.user.clone(),
    auth,
    forward_agent: saved.forward_agent,
    forwards,
    jump,
    // 记住了密码的条目带上凭据键：连接期密码路可按它自动取已存凭据
    credential_id: saved.remember_password.then(|| saved.id.clone()),
  })
}

/// 保存的转发规则 → 会话用的流（字段一一对应）。
fn flow_of_saved(saved: &SavedForward) -> zhimaterm_session::ForwardFlow {
  match saved {
    SavedForward::Local {
      bind_addr,
      bind_port,
      target_host,
      target_port,
    } => zhimaterm_session::ForwardFlow::Local {
      bind_addr: bind_addr.clone(),
      bind_port: *bind_port,
      target_host: target_host.clone(),
      target_port: *target_port,
    },
    SavedForward::Remote {
      remote_port,
      target_host,
      target_port,
    } => zhimaterm_session::ForwardFlow::Remote {
      remote_port: *remote_port,
      target_host: target_host.clone(),
      target_port: *target_port,
    },
    SavedForward::Socks { bind_addr, bind_port } => zhimaterm_session::ForwardFlow::Socks {
      bind_addr: bind_addr.clone(),
      bind_port: *bind_port,
    },
  }
}
