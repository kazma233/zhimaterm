//! 保存的 SSH 连接。
//!
//! 只存"能重建连接"的字段：主机、端口、用户、认证信息引用（agent 开关、私钥
//! 路径）、agent 转发开关与转发规则。口令与私钥口令**不落盘**（ADR-002），
//! 所以文件被读走也拿不到凭据；需要口令的连接每次连接时再输。
//!
//! 写入走临时文件 + rename：读到的要么是旧内容、要么是新内容，不会出现写坏的半个文件。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 保存下来的认证信息引用（ADR-010）。刻意没有口令字段。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedCredentials {
    /// 本地 ssh-agent 是否参与认证；缺省视为参与（与表单默认一致）
    #[serde(default = "yes")]
    pub use_agent: bool,
    /// 私钥文件路径；None = 没有私钥素材
    #[serde(default)]
    pub key_path: Option<String>,
}

impl Default for SavedCredentials {
    fn default() -> Self {
        Self {
            use_agent: true,
            key_path: None,
        }
    }
}

fn yes() -> bool {
    true
}

impl SavedCredentials {
    /// 这条连接只有密码认证（没有 agent、没有私钥）：连接前必须先拿到密码，
    /// 列表行因此标「需输入」——记住密码的条目除外（ADR-018：钥匙串在连接
    /// 确需密码时自动取用），标记的取舍在 `saved_row`，那里同时看得到
    /// `remember_password`。
    pub fn password_only(&self) -> bool {
        !self.use_agent && self.key_path.is_none()
    }

    /// 列表行里的素材摘要。
    pub fn label(&self) -> String {
        let key = self.key_path.is_some().then_some("私钥");
        match (self.use_agent, key) {
            (true, Some(_)) => "ssh-agent + 私钥".into(),
            (true, None) => "ssh-agent".into(),
            (false, Some(_)) => "私钥".into(),
            // 与旧版的「密码」条目同名：这是用户当时的显式选择，不该被改名
            (false, None) => "密码".into(),
        }
    }
}

/// 一条保存的转发规则（ADR-011），字段与 `ForwardRule` 一一对应。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedForward {
    Local {
        bind_addr: String,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    },
    Remote {
        remote_port: u16,
        target_host: String,
        target_port: u16,
    },
    /// 动态 SOCKS5 转发（ADR-038）
    Socks { bind_addr: String, bind_port: u16 },
}

/// 保存的跳板机（ADR-020）：与 `SshJump` 对应，凭据只存引用（agent 开关与
/// 私钥路径）；跳板密码不写钥匙串（记住密码只覆盖目标），口令类不落盘。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedJump {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub credentials: SavedCredentials,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SavedConnection {
    /// 条目 id（ADR-018 修订）：随机生成、与身份无关。列表更新与钥匙串凭据
    /// 都按它定位——同身份条目允许并存，改主机/端口/用户不需要任何迁移。
    pub id: String,
    /// 用户起的名字（ADR-048）：列表行与新建标签的标题；None 时显示
    /// `用户@主机:端口`。不参与条目身份
    #[serde(default)]
    pub name: Option<String>,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub credentials: SavedCredentials,
    /// agent 转发开关（ADR-012）
    #[serde(default)]
    pub forward_agent: bool,
    #[serde(default)]
    pub forwards: Vec<SavedForward>,
    /// 记住密码（ADR-018）：引用标记——钥匙串里有这条（按 id）的凭据，
    /// 明文永不落盘
    #[serde(default)]
    pub remember_password: bool,
    /// 跳板机（ADR-020）：默认不走跳板
    #[serde(default)]
    pub jump: Option<SavedJump>,
}

/// 新条目的 id：随机（uuid v4），不携带任何身份信息。
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 读取兼容旧格式（ADR-010 之前的 `auth: {kind}`）：按 ADR-010 的迁移规则
/// 折算成素材引用，保存时写新格式。
impl<'de> Deserialize<'de> for SavedConnection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            id: Option<String>,
            #[serde(default)]
            name: Option<String>,
            host: String,
            port: u16,
            user: String,
            #[serde(default)]
            auth: Option<LegacyAuth>,
            #[serde(default)]
            credentials: Option<SavedCredentials>,
            #[serde(default)]
            forward_agent: bool,
            #[serde(default)]
            forwards: Vec<SavedForward>,
            #[serde(default)]
            remember_password: bool,
            #[serde(default)]
            jump: Option<SavedJump>,
        }

        // 旧格式的互斥认证方式；kind 的取值与旧版写盘的 snake_case 一致
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum LegacyAuth {
            Agent,
            PrivateKey { path: String },
            Password,
        }

        let raw = Raw::deserialize(deserializer)?;
        // 迁移规则（ADR-010）：旧 private_key 是用户的显式选择，不让 agent 抢先；
        // 旧 agent / password 都回到"agent 默认参与"
        let credentials = match raw.auth {
            Some(LegacyAuth::Agent) => SavedCredentials {
                use_agent: true,
                key_path: None,
            },
            // 旧「密码」条目按原样保留：有人连上时不做无谓的 agent 尝试，
            // 列表里也照旧显示「密码」而不是被折成 ssh-agent（用户找不到自己的条目）
            Some(LegacyAuth::Password) => SavedCredentials {
                use_agent: false,
                key_path: None,
            },
            Some(LegacyAuth::PrivateKey { path }) => SavedCredentials {
                use_agent: false,
                key_path: Some(path),
            },
            None => raw.credentials.unwrap_or_default(),
        };
        // 旧存档没有 id：载入时补发一个。它的已存凭据当年按身份键写进钥匙串，
        // 那批条目不再读取（ADR-018 修订），remember 标记一并作废——当作都没存过
        let legacy = raw.id.is_none();
        Ok(Self {
            id: raw.id.unwrap_or_else(new_id),
            name: raw.name,
            host: raw.host,
            port: raw.port,
            user: raw.user,
            credentials,
            forward_agent: raw.forward_agent,
            forwards: raw.forwards,
            remember_password: raw.remember_password && !legacy,
            jump: raw.jump,
        })
    }
}

impl SavedConnection {
    /// 列表里显示的一行：`用户@主机:端口`。
    pub fn target_label(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }

    /// 给人看的标题：起过名字用名字，否则 `用户@主机:端口`。
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.target_label())
    }

    /// 连接身份判定：同一个人连同一台机器的同一端口。运行中会话与保存列表
    /// 的对应关系（转发规则回写，ADR-038）仍按它判定；列表的增删改已不经过
    /// 身份（ADR-018 修订：条目以 id 为键）。
    pub fn same_identity(&self, host: &str, port: u16, user: &str) -> bool {
        self.host == host && self.port == port && self.user == user
    }
}

/// 保存的连接集合。唯一所有者是它自己，落盘在配置目录。
pub struct SavedConnections {
    path: PathBuf,
    items: Vec<SavedConnection>,
}

impl SavedConnections {
    /// 从配置目录读取。文件不存在就是空；解析失败要报错，不静默覆盖用户已有的数据。
    pub fn load() -> Result<Self> {
        let path = crate::config_paths::config_dir()?.join("connections.json");
        Self::load_at(path)
    }

    pub fn load_at(path: PathBuf) -> Result<Self> {
        let items = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("解析保存的连接失败：{}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取保存的连接失败：{}", path.display()));
            }
        };
        Ok(Self { path, items })
    }

    pub fn items(&self) -> &[SavedConnection] {
        &self.items
    }

    /// 保存一条：id 已在列表里就原位替换（编辑保存），否则追加（新建保存）。
    /// 不做任何按身份的覆盖或合并——重复条目是用户的显式选择。
    pub fn save(&mut self, connection: SavedConnection) -> Result<()> {
        match self
            .items
            .iter_mut()
            .find(|existing| existing.id == connection.id)
        {
            Some(existing) => *existing = connection,
            None => self.items.push(connection),
        }
        self.persist()
    }

    pub fn remove(&mut self, index: usize) -> Result<()> {
        if index >= self.items.len() {
            return Ok(());
        }
        self.items.remove(index);
        self.persist()
    }

    /// 按连接身份（host/port/user）找保存的连接。运行中增删转发后回写用它
    /// 判定「这条会话是否对应一条已保存的连接」（ADR-038）。
    pub fn find_target(&self, host: &str, port: u16, user: &str) -> Option<&SavedConnection> {
        self.items
            .iter()
            .find(|item| item.same_identity(host, port, user))
    }

    /// 就地更新一条已保存连接的转发规则（ADR-038），身份不变，不新增条目。
    pub fn replace_forwards(
        &mut self,
        host: &str,
        port: u16,
        user: &str,
        forwards: Vec<SavedForward>,
    ) -> Result<()> {
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.same_identity(host, port, user))
        else {
            return Ok(());
        };
        item.forwards = forwards;
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let json = serde_json::to_vec_pretty(&self.items).context("序列化保存的连接失败")?;
        crate::config_paths::write_atomic(&self.path, &json)
            .with_context(|| format!("保存连接失败：{}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::{SavedConnection, SavedConnections, SavedCredentials, SavedForward, SavedJump};

    fn connection(host: &str, user: &str, credentials: SavedCredentials) -> SavedConnection {
        SavedConnection {
            id: super::new_id(),
            name: None,
            host: host.to_owned(),
            port: 22,
            user: user.to_owned(),
            credentials,
            forward_agent: false,
            forwards: Vec::new(),
            remember_password: false,
            jump: None,
        }
    }

    fn agent_credentials() -> SavedCredentials {
        SavedCredentials {
            use_agent: true,
            key_path: None,
        }
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zhimaterm-connections-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("connections.json")
    }

    #[test]
    fn saved_connections_survive_a_round_trip() {
        let path = temp_path("round-trip");
        let mut store = SavedConnections::load_at(path.clone()).expect("空文件应能加载");
        assert!(store.items().is_empty());

        let mut with_forwards = connection("a.example.com", "root", agent_credentials());
        with_forwards.forward_agent = true;
        with_forwards.forwards = vec![
            SavedForward::Local {
                bind_addr: "127.0.0.1".into(),
                bind_port: 8080,
                target_host: "db.internal".into(),
                target_port: 5432,
            },
            SavedForward::Remote {
                remote_port: 9999,
                target_host: "127.0.0.1".into(),
                target_port: 3000,
            },
        ];
        store.save(with_forwards).expect("保存应成功");
        let mut with_flags = connection(
            "b.example.com",
            "deploy",
            SavedCredentials {
                use_agent: false,
                key_path: Some("~/.ssh/id_ed25519".into()),
            },
        );
        // 记住密码（ADR-018）只存引用标记
        with_flags.remember_password = true;
        store.save(with_flags).expect("保存应成功");

        let reloaded = SavedConnections::load_at(path).expect("重新加载应成功");
        assert_eq!(reloaded.items(), store.items());
        assert_eq!(reloaded.items()[0].forwards.len(), 2);
        assert!(reloaded.items()[1].remember_password);
        assert_eq!(
            reloaded.items()[1].target_label(),
            "deploy@b.example.com:22"
        );
    }

    /// 跳板机（ADR-020）随连接持久化：引用字段（agent 开关 + 私钥路径）往返
    /// 保留；旧存档没有 jump 字段时按 None 加载，不会报错。
    #[test]
    fn jump_survives_a_round_trip_and_absent_means_none() {
        let mut with_jump = connection(
            "inner.example.com",
            "deploy",
            SavedCredentials {
                use_agent: false,
                key_path: None,
            },
        );
        with_jump.jump = Some(SavedJump {
            host: "bastion.example.com".into(),
            port: 2222,
            user: "ops".into(),
            credentials: SavedCredentials {
                use_agent: true,
                key_path: Some("~/.ssh/bastion_key".into()),
            },
        });
        let json = serde_json::to_string(&with_jump).expect("序列化应成功");
        let reloaded: SavedConnection = serde_json::from_str(&json).expect("反序列化应成功");
        assert_eq!(reloaded.jump, with_jump.jump);

        // 旧存档（ADR-020 之前）没有 jump 字段
        let legacy = r#"{
            "host": "a.example.com", "port": 22, "user": "root",
            "credentials": { "use_agent": true, "key_path": null }
        }"#;
        let legacy: SavedConnection = serde_json::from_str(legacy).expect("旧存档应能加载");
        assert!(legacy.jump.is_none());
    }

    /// 连接名称（ADR-048）随条目持久化；旧存档没有 name 字段按「没起名」读，
    /// 显示标题回退到 `用户@主机:端口`。名字只是显示标签：改名走编辑保存
    /// （同 id 原位替换），名字变化不影响条目身份。
    #[test]
    fn name_round_trips_and_display_falls_back_to_target() {
        let path = temp_path("name");
        let mut store = SavedConnections::load_at(path.clone()).unwrap();
        let mut named = connection("a.example.com", "root", agent_credentials());
        named.name = Some("生产机".into());
        store.save(named).unwrap();
        store
            .save(connection("b.example.com", "root", agent_credentials()))
            .unwrap();

        let reloaded = SavedConnections::load_at(path.clone()).unwrap();
        assert_eq!(reloaded.items()[0].name.as_deref(), Some("生产机"));
        assert_eq!(reloaded.items()[0].display_name(), "生产机");
        assert_eq!(reloaded.items()[1].name, None);
        assert_eq!(reloaded.items()[1].display_name(), "root@b.example.com:22");

        let mut renamed = connection("a.example.com", "root", agent_credentials());
        renamed.id = store.items()[0].id.clone();
        renamed.name = Some("预发".into());
        store.save(renamed).unwrap();
        assert_eq!(store.items().len(), 2, "同 id 改名是原位替换");
        assert_eq!(store.items()[0].name.as_deref(), Some("预发"));

        let legacy = r#"{
            "host": "a.example.com", "port": 22, "user": "root",
            "credentials": { "use_agent": true, "key_path": null }
        }"#;
        let legacy: SavedConnection = serde_json::from_str(legacy).expect("旧存档应能加载");
        assert!(legacy.name.is_none());
        assert_eq!(legacy.display_name(), "root@a.example.com:22");
    }

    /// 同一条目的 id 原位替换（编辑保存）；不同条目（id 不同）即使身份完全
    /// 相同也各自成条——重复条目是允许的，身份不参与列表增删（ADR-018 修订）。
    #[test]
    fn saving_replaces_by_id_and_appends_others() {
        let mut store = SavedConnections::load_at(temp_path("replace")).unwrap();
        let first = connection("a.example.com", "root", agent_credentials());
        store.save(first).unwrap();

        // 编辑保存：同一 id，改素材 → 原位替换，不新增
        let mut edited = connection("a.example.com", "root", agent_credentials());
        edited.id = store.items()[0].id.clone();
        edited.credentials = SavedCredentials {
            use_agent: false,
            key_path: Some("/tmp/key".into()),
        };
        store.save(edited).unwrap();
        assert_eq!(store.items().len(), 1, "同 id 应原位替换");
        assert_eq!(
            store.items()[0].credentials.key_path,
            Some("/tmp/key".into())
        );

        // 新建保存：身份与已有条目完全相同也追加成新条目
        let twin = connection("a.example.com", "root", agent_credentials());
        store.save(twin).unwrap();
        assert_eq!(store.items().len(), 2, "不同 id 的同身份条目应并存");
    }

    /// 旧存档条目没有 id：载入补发，且 remember 标记作废（那批凭据按身份键
    /// 存的，不再读取，当作都没存过——ADR-018 修订）；带 id 的条目标记照常。
    #[test]
    fn legacy_entries_get_a_fresh_id_and_lose_the_remember_flag() {
        let json = r#"[
            {"host":"a.example.com","port":22,"user":"root","remember_password":true,
             "credentials":{"use_agent":true,"key_path":null}},
            {"id":"explicit-id","host":"b.example.com","port":22,"user":"root",
             "remember_password":true,
             "credentials":{"use_agent":true,"key_path":null}}
        ]"#;
        let path = temp_path("legacy-id");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json).unwrap();
        let store = SavedConnections::load_at(path).unwrap();
        let items = store.items();

        assert!(!items[0].remember_password, "旧条目的 remember 应作废");
        assert!(
            uuid::Uuid::parse_str(&items[0].id).is_ok(),
            "补发的 id 应是 uuid：{}",
            items[0].id
        );
        assert_eq!(items[1].id, "explicit-id", "自带 id 的条目原样保留");
        assert!(items[1].remember_password, "带 id 条目的 remember 照常");
    }

    #[test]
    fn removing_one_keeps_the_rest() {
        let path = temp_path("remove");
        let mut store = SavedConnections::load_at(path.clone()).unwrap();
        store
            .save(connection("a.example.com", "root", agent_credentials()))
            .unwrap();
        store
            .save(connection(
                "b.example.com",
                "root",
                SavedCredentials {
                    use_agent: false,
                    key_path: None,
                },
            ))
            .unwrap();

        store.remove(0).unwrap();
        assert_eq!(store.items().len(), 1);
        assert_eq!(store.items()[0].host, "b.example.com");

        let reloaded = SavedConnections::load_at(path).unwrap();
        assert_eq!(reloaded.items(), store.items());
    }

    /// 文件里不能出现任何口令类字段：这是 ADR-002 的硬约束，用测试盯住。
    #[test]
    fn the_saved_file_never_contains_secrets() {
        let path = temp_path("no-secrets");
        let mut store = SavedConnections::load_at(path.clone()).unwrap();
        store
            .save(connection(
                "a.example.com",
                "root",
                SavedCredentials {
                    use_agent: false,
                    key_path: None,
                },
            ))
            .unwrap();
        store
            .save(connection("b.example.com", "root", agent_credentials()))
            .unwrap();

        let json = std::fs::read_to_string(&path).unwrap();
        // 按字段名检查：素材字段的取值可能含 password 字样，所以不能只搜这个词
        for forbidden in [
            "\"password\":",
            "\"passphrase\":",
            "\"secret\":",
            "\"token\":",
            "\"credential\":",
        ] {
            assert!(
                !json.contains(forbidden),
                "保存的连接里不该出现字段 {forbidden}：{json}"
            );
        }
    }

    /// 旧格式（互斥 `auth: {kind}`）按 ADR-010 规则迁移：private_key 关掉 agent
    /// 参与（那是用户的显式选择），agent / password 回到 agent 默认参与。
    #[test]
    fn legacy_auth_kinds_migrate_to_stacked_credentials() {
        let dir =
            std::env::temp_dir().join(format!("zhimaterm-conn-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("connections.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"[
                {"host":"a.example.com","port":22,"user":"root","auth":{"kind":"agent"}},
                {"host":"b.example.com","port":22,"user":"root","auth":{"kind":"private_key","path":"~/.ssh/id_ed25519"}},
                {"host":"c.example.com","port":22,"user":"root","auth":{"kind":"password"}}
            ]"#,
        )
        .unwrap();

        let store = SavedConnections::load_at(path.clone()).unwrap();
        let items = store.items();
        assert_eq!(
            items[0].credentials,
            SavedCredentials {
                use_agent: true,
                key_path: None
            }
        );
        assert_eq!(
            items[1].credentials,
            SavedCredentials {
                use_agent: false,
                key_path: Some("~/.ssh/id_ed25519".into())
            }
        );
        // 旧「密码」条目保持"仅密码"：不被折成 ssh-agent
        assert_eq!(
            items[2].credentials,
            SavedCredentials {
                use_agent: false,
                key_path: None
            }
        );
        assert!(items[2].credentials.password_only());
        assert_eq!(items[2].credentials.label(), "密码");

        // 迁移只发生在读取：重新保存写的是新格式
        let first = items[0].clone();
        drop(store);
        let mut store = SavedConnections::load_at(path.clone()).unwrap();
        store.save(first).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(!json.contains("\"kind\": \"agent\""), "{json}");
        assert!(json.contains("\"use_agent\": true"), "{json}");
    }
}
