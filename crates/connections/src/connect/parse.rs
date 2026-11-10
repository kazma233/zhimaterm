//! 连接表单的文本解析：目标串（`[user@]host[:port]`）、字段合并与 `~` 展开。
//! 纯函数，不接触表单与 gpui。端口解析在 `crate::forward_draft`（连接表单
//! 与转发表单共用同一份）。

use std::path::PathBuf;

use super::TargetPart;

/// 主机字段里粘贴进来的目标串拆成三段：`[user@]host[:port]`。
///
/// 允许粘贴是照 OpenSSH 的目标语法（`man ssh`：destination 写作 `[user@]hostname`
/// 或 `ssh://[user@]hostname[:port]`）；但**不做静默取舍**——拆出来的用户名与端口
/// 由调用方并入各自字段，对不上时报错，不偷偷丢掉任何一段。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Destination {
    pub(super) user: Option<String>,
    pub(super) host: String,
    pub(super) port: Option<u16>,
}

/// 跳板目标框的规则：`主机[:端口]`，不带用户名。校验与提交共用这一份——
/// `user@` 不静默并入「跳板用户」，直接指过去让用户自己定。
pub(super) fn split_jump_target(input: &str) -> Result<Destination, String> {
    let destination = split_destination(input)?;
    if destination.user.is_some() {
        return Err("不要带用户名：请把 @ 前的内容填到「跳板用户」字段".into());
    }
    Ok(destination)
}

/// 纯拆分，不触碰表单：把 `[user@]host[:port]` 解析成三段。
/// 没有可拆的内容时 user/port 为 None，host 原样（去首尾空白）。
pub(super) fn split_destination(input: &str) -> Result<Destination, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("请填写主机".into());
    }

    let (user, rest) = match trimmed.rsplit_once('@') {
        Some((user, host)) => {
            let user = user.trim();
            if user.is_empty() {
                return Err("「@」前没有用户名：填上用户名，或删掉这个 @".into());
            }
            let host = host.trim();
            if host.is_empty() {
                return Err("请填写主机".into());
            }
            (Some(user.to_owned()), host)
        }
        None => (None, trimmed),
    };

    // 方括号形式用来写 IPv6 的端口，例如 [::1]:2222；
    // 冒号恰好一个才按 `host:port` 拆（裸 IPv6 有多个冒号，整体当主机）
    let (host, port_text) = if let Some(inner) = rest.strip_prefix('[') {
        match inner.split_once(']') {
            Some((address, tail)) => {
                let port = tail
                    .strip_prefix(':')
                    .map(str::trim)
                    .filter(|port| !port.is_empty());
                (address.trim().to_owned(), port.map(str::to_owned))
            }
            None => (rest.to_owned(), None),
        }
    } else if rest.matches(':').count() == 1 {
        match rest.rsplit_once(':') {
            Some((address, tail)) => (address.trim().to_owned(), Some(tail.trim().to_owned())),
            None => (rest.to_owned(), None),
        }
    } else {
        (rest.to_owned(), None)
    };

    if host.is_empty() {
        return Err("请填写主机".into());
    }
    let port = match port_text {
        Some(text) => {
            let port: u16 = text
                .parse()
                .map_err(|_| format!("端口不是 1-65535 的数字：{text}"))?;
            if port == 0 {
                return Err("端口不能为 0".into());
            }
            Some(port)
        }
        None => None,
    };

    Ok(Destination { user, host, port })
}

/// 展开时把一个值并入目标字段的结果。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MergeOutcome {
    /// 目标字段为空：填入
    Fill,
    /// 目标字段已有相同的值：不必动
    Unchanged,
    /// 目标字段已有不同的值：报错，不静默取舍
    Conflict,
}

pub(super) fn merge_field(existing: &str, incoming: &str) -> MergeOutcome {
    let existing = existing.trim();
    if existing.is_empty() {
        MergeOutcome::Fill
    } else if existing == incoming.trim() {
        MergeOutcome::Unchanged
    } else {
        MergeOutcome::Conflict
    }
}

/// 解析「主机 / 端口 / 用户」三个字段。主机字段只填主机地址——粘贴进来的
/// `user@`、`:port` 在提交前已由 [`ConnectForm::expand_host`] 拆进各自字段，
/// 这里只做校验。错误带上是哪一段没过，调用方映射到自己的字段。
pub(super) fn parse_target(
    host: &str,
    port: &str,
    user: &str,
) -> Result<(String, u16, String), (TargetPart, String)> {
    let host = host.trim();
    if host.is_empty() {
        return Err((TargetPart::Host, "请填写主机".into()));
    }
    // 兜底：正常路径上展开已经处理过；越过展开直接到这里说明上游漏了一步
    if host.contains('@') {
        return Err((
            TargetPart::Host,
            "主机里不要带用户名：请把 @ 前的内容填到「用户」字段".into(),
        ));
    }

    let port_text = port.trim();
    let port_text = if port_text.is_empty() {
        "22"
    } else {
        port_text
    };
    let port: u16 = port_text.parse().map_err(|_| {
        (
            TargetPart::Port,
            format!("端口不是 1-65535 的数字：{port_text}"),
        )
    })?;
    if port == 0 {
        return Err((TargetPart::Port, "端口不能为 0".into()));
    }

    let user = user.trim();
    if user.is_empty() {
        return Err((TargetPart::User, "请填写用户名".into()));
    }

    Ok((host.to_owned(), port, user.to_owned()))
}

/// 展开开头的 `~`：用户按习惯写路径，应用不去猜 shell 的展开规则。
/// 主目录来源与配置目录一致（`config_paths::home_dir`）。
pub(super) fn expand_home(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    match zhimaterm_base::config_paths::home_dir() {
        Some(home) => home.join(rest),
        None => PathBuf::from(path),
    }
}
