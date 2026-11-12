//! 认证装配与保存计划：表单 → `SshAuth`/`SshJump`，编辑态 → `SavePlan`。
//! 口令只随出口交出去一次，不落任何配置。

use std::path::PathBuf;

use gpui::{App, Context};

use crate::connections::{SavedConnection, SavedConnections};
use zhimaterm_session::{SshAuth, SshJump};

use super::parse::expand_home;
use super::{ConnectForm, Field, FormError};

/// 钥匙串失败进底栏（ADR-048 §5 的动作级错误）：保存/迁移是「保存」这个动作
/// 的一部分，不属于哪个输入框；补法是重试或修系统。
fn keychain_error(error: anyhow::Error) -> FormError {
  FormError::global(format!("{error:#}"))
}

/// 认证信息（含运行期口令）→ 会话凭据。口令只随会话配置交出去一次，调用方不留副本。
pub(super) fn form_auth(form: &ConnectForm, cx: &App) -> Result<SshAuth, FormError> {
  let key_path = form.key_path.read(cx).text().trim().to_owned();
  let passphrase = form.key_passphrase.read(cx).text().to_owned();
  let password = form.password.read(cx).text().to_owned();

  // 静态素材可以为空（ADR-048 修订）：连接时前面的路都没成，认证会向
  // UI 要一把临时密码
  Ok(SshAuth {
    use_agent: form.use_agent,
    key_path: if key_path.is_empty() {
      PathBuf::new()
    } else {
      expand_home(&key_path)
    },
    key_passphrase: (!passphrase.is_empty()).then_some(passphrase),
    password: (!password.is_empty()).then_some(password),
  })
}

/// 表单的跳板配置（ADR-020）→ 会话用的 SshJump。关闭时 None；
/// 开启时口令只随会话配置交出去一次，与主表单同一规则。
pub(super) fn form_jump(form: &ConnectForm, cx: &App) -> Result<Option<SshJump>, FormError> {
  if !form.jump_enabled {
    return Ok(None);
  }
  let key_path = form.jump.key_path.read(cx).text().trim().to_owned();
  let passphrase = form.jump.key_passphrase.read(cx).text().to_owned();
  let password = form.jump.password.read(cx).text().to_owned();

  let auth = SshAuth {
    use_agent: form.jump_use_agent,
    key_path: if key_path.is_empty() {
      PathBuf::new()
    } else {
      expand_home(&key_path)
    },
    key_passphrase: (!passphrase.is_empty()).then_some(passphrase),
    password: (!password.is_empty()).then_some(password),
  };
  if auth.is_empty() {
    return Err(FormError::at(
      Field::JumpPassword,
      "跳板机没有认证信息：请启用 ssh-agent、填写私钥文件或密码",
    ));
  }
  let (host, port, user) = form.parse_jump_target(cx)?;
  Ok(Some(SshJump { host, port, user, auth }))
}

/// 保存计划（GLOSSARY）：编辑态与正在编辑的旧条目一次折算出的保存决定。
/// 决策错误已在 [`ConnectForm::save`] 里贴到表单错误面，这里只携带执行所需
/// 的事实，执行不回看表单与外壳；口令只随计划交到钥匙串一步。
pub struct SavePlan {
  /// 要保存的连接（id 已定：编辑沿旧 id，新建新发），口令永不入内。
  pub(super) entry: SavedConnection,
  /// 手输的密码；None = 不碰钥匙串（空框沿用已存的那份，或没记住）。
  /// 凭据按条目 id 挂钥匙串（ADR-018 修订），没有身份迁移这回事。
  pub(super) password: Option<String>,
}

impl SavePlan {
  /// 钥匙串步（ADR-018：先动，成功才落盘）——写不进去就中止保存，避免
  /// 「以为记住了」。
  pub fn run_keychain(&self) -> Result<(), FormError> {
    if let Some(password) = &self.password
      && let Err(error) = crate::keychain::save(&self.entry.id, password)
    {
      return Err(keychain_error(error));
    }
    Ok(())
  }

  /// 列表步：钥匙串成功后落盘。编辑保存按 id 原位替换，新建保存追加
  /// （ADR-018 修订：不做任何按身份的覆盖或合并）。
  pub fn apply_to_store(&self, store: &mut SavedConnections) -> Result<(), FormError> {
    store
      .save(self.entry.clone())
      .map_err(|error| FormError::global(format!("保存失败：{error:#}")))
  }
}
impl ConnectForm {
  /// 保存出口：编辑态与正在编辑的旧条目折算成保存计划（GLOSSARY）。前奏
  /// （清错误、主机展开、装配校验）与钥匙串分支矩阵都在这里；决策错误自己
  /// 贴到表单错误面并返回 None，外壳据此止步。`previous` 是外壳按下标取的
  /// 正在编辑的条目，不做筛选——哪些情形在乎旧条目由表单决定。
  pub fn save(&mut self, cx: &mut Context<Self>, previous: Option<&SavedConnection>) -> Option<SavePlan> {
    // 每轮提交从干净状态重算（ADR-048 §5）：上一轮的字段/草稿/底栏/勾选行
    // 错误全部清掉
    self.clear_errors(cx);
    // 回车提交不经过失焦，这里再展开一次；失败就不落盘
    if let Err(error) = self.expand_host(cx) {
      self.fail(error, cx);
      return None;
    }
    let mut entry = match self.saved_entry(cx) {
      Ok(entry) => entry,
      Err(error) => {
        self.fail(error, cx);
        return None;
      }
    };
    // ADR-018（2026-10-29 修订）：输入即保存——密码框非空就按条目 id 写
    // 钥匙串；空框绝不写（沿用已存的那份，或本来就没有）。条目的
    // remember 标记是派生值：「钥匙串里有这条的凭据」。凭据跟 id 走，
    // 改主机/端口/用户不需要任何迁移。
    let typed = self.password_text(cx);
    entry.remember_password = !typed.is_empty() || previous.is_some_and(|saved| saved.remember_password);
    let password = (!typed.is_empty()).then_some(typed);
    // 保存成功后表单转入「正在编辑这条」：再存原位替换，连接路径也拿得到 id
    self.editing_id = Some(entry.id.clone());
    Some(SavePlan { entry, password })
  }
}
