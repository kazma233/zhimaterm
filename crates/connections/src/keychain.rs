//! OS 安全凭据存储：记住的密码（ADR-018）。
//!
//! 条目键是保存连接的随机 id（service `zhimaterm` + 账号 = 条目 id）：
//! 一条连接一条凭据，凭据随条目走——改主机/端口/用户不需要任何迁移，
//! 同身份的重复条目也各有各的凭据。钥匙串只在保存连接（写）、密码框内
//! 「显示」与连接确需密码（读，含连接期自动应答；载入表单不读，只设
//! 「已存有密码」的星号标识态）、忘掉连接（删）被触碰；读不到一律回退
//! 手输，不阻断任何流程。
//!
//! 单测不碰真实钥匙串（工作区规范）：真实读写删回路由 `ZHIMATERM_KEYCHAIN=1`
//! 门控的忽略测试覆盖，需要时手动跑。

use anyhow::{Context, Result};

const SERVICE: &str = "zhimaterm";

/// 写入（保存连接时）。写失败要如实报给调用方——用户意图是记住，
/// 静默降级成「没记住」会让下次连接莫名要手输。
pub fn save(id: &str, password: &str) -> Result<()> {
    entry(id)?
        .set_password(password)
        .with_context(|| format!("写入钥匙串失败：{id}"))
}

/// 读取（表单「显示」、连接确需密码时）。None = 没有记住（条目不存在或
/// 后端不可用）：按没记住处理，回退手输。
pub fn load(id: &str) -> Result<Option<String>> {
    let entry = entry(id)?;
    match entry.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => {
            // 后端不可用（锁定/缺 Secret Service）：与「没记住」同路，
            // 但原因要进日志口径的上下文里，便于排查
            Err(anyhow::anyhow!("读取钥匙串失败（{id}）：{error}"))
        }
    }
}

/// 删除（忘掉连接时）。条目本来就不在不算失败。
pub fn delete(id: &str) -> Result<()> {
    match entry(id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow::anyhow!("删除钥匙串条目失败（{id}）：{error}")),
    }
}

fn entry(id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, id).with_context(|| format!("创建钥匙串条目失败：{id}"))
}

#[cfg(test)]
mod tests {
    use super::{delete, load, save};

    /// 真实钥匙串的读写删回路（ADR-018）。默认不跑——单测不该动真实凭据
    /// 存储；手动跑：`ZHIMATERM_KEYCHAIN=1 cargo test -- --ignored keychain`
    /// macOS 首次运行可能弹系统授权框。
    #[test]
    #[ignore = "写真实系统钥匙串，需 ZHIMATERM_KEYCHAIN=1 显式开启"]
    fn round_trip_through_the_real_store() {
        if std::env::var("ZHIMATERM_KEYCHAIN").as_deref() != Ok("1") {
            eprintln!("未设置 ZHIMATERM_KEYCHAIN=1，跳过");
            return;
        }
        let id = crate::connections::new_id();
        // 清掉历史遗留，从已知状态开始
        let _ = delete(&id);

        assert_eq!(load(&id).unwrap(), None, "初始应无条目");
        save(&id, "probe-密码").unwrap();
        assert_eq!(
            load(&id).unwrap().as_deref(),
            Some("probe-密码"),
            "写入后应能读回"
        );
        delete(&id).unwrap();
        assert_eq!(load(&id).unwrap(), None, "删除后应读不到");
        // 再删一次不报错（条目不存在不算失败）
        delete(&id).unwrap();
    }
}
