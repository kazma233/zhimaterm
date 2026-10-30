# ADR-010:SSH 认证素材叠加与 agent 参与

状态:已接受(2026-10)

## 背景

原模型是互斥三选一(`ssh-agent` / 私钥文件 / 密码),且 agent 失败不降级到密码。
与 `ssh` 命令的习惯不符:agent 是环境能力(系统里有就自动参与),私钥与密码是
补充素材,三者从来不是互斥选项。用户在本地跑着 agent 时,还要显式"选 agent
方式",体验割裂(见 `../context/mobaxterm-gap-and-priorities.md` 排序过程)。

## 决策

1. `SshAuth` 从枚举改为素材集合:`use_agent`(默认开)、`key_path` + 可选口令、
   `password`。三者可同时存在。
2. 尝试顺序对齐 OpenSSH:**agent → 指定私钥 → 密码(keyboard-interactive 先、
   password 后)**。服务端声明的认证方法(SSH userauth none 探测)用于跳过
   明确不支持的方式,不浪费 MaxAuthTries 次数。
3. **全部失败时逐项报告原因**,不做静默降级。ADR-002 的"agent 失败不退化到
   密码"在此重新表述为:**尝试顺序公开、失败原因全量可见**——素材是用户自己
   提供的,逐一尝试不属于静默 fallback;防爆破断开时保留人话提示。
4. `use_agent` 开关随连接持久化,等价于 OpenSSH `IdentityAgent none`(本地 agent
   钥匙多、不想让某台服务器碰)。关闭时 agent 完全不参与——注意与 `-a`/`ForwardAgent`
   无关:那一条管的是"把 agent 转发给远端",由会话级的 agent 转发开关承担。
5. agent 连接路径不做配置项:靠系统约定自动发现(`SSH_AUTH_SOCK` /
   Pageant / `\\.\pipe\openssh-ssh-agent`,由 libssh2 按平台处理);
   连不上时在认证失败信息中说明,不提供地址输入框。

## 持久化与迁移

`connections.json` 改存素材字段(`use_agent`、可选 `key_path`),口令照旧不落盘。
旧格式的 `auth: {kind}` 迁移规则:**三种都保留用户当时的显式选择**——
`agent` → `use_agent: true`;`private_key` → `use_agent: false` + `key_path`;
`password` → `use_agent: false`(即"密码"条目,连接时不做无谓的 agent 尝试,
列表里也照旧显示「密码」)。读取时迁移,保存时写新格式。

> 初稿曾把旧 `password` 折成 `use_agent: true`(理由:"回到 agent 默认参与"),
> 实测造成用户困惑——旧密码条目在列表里被改名为 `ssh-agent`,用户找不到
> 自己的连接。素材叠加是给新连接的自由,不该改写旧条目记录的用户意图。

## 实现中踩实的硬约束(2026-10,e2e 实测)

**agent 素材不能用 `Session::userauth_agent`**:该便捷方法每次调用都新建
一个 `Agent` 并在返回时丢弃,而非阻塞会话下 libssh2 把公钥认证停在会话级
中途状态(`userauth_pblc_state`),EAGAIN 后再次调用会从旧状态恢复、却拿到
一个还没装载钥匙的新 Agent——实测直接 SIGSEGV(libssh2 1.11.x,
`agent_sign` 空指针;旧代码的 EAGAIN 重试一直有这颗雷,只是从未在带
agent 的环境下跑过 e2e)。改为自持一个 Agent 跨全部重试:
connect / list_identities / identities / userauth 每一步都回到同一个 Agent
上继续,与 libssh2 的公开非阻塞约定一致(见 `ssh/auth.rs::authenticate_with_agent`;传输层拆分见 ADR-004 2026-10-29 修订)。

新增 e2e 门控用例覆盖素材叠加:agent+私钥+密码同时提供时按顺序落到可用者、
两种素材都失败时逐项报原因。

## 后果

- 表单移除"认证方式"单选,字段改为可选素材 + agent 开关。
- agent 转发(ADR-012)与认证解耦:转发的是本地 agent 这个环境能力,
  与本次用什么素材认证无关。
- e2e 认证用例改为按素材组合覆盖(agent+key 叠加、密码叠加等)。
