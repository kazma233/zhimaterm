# SSH 转发与跳板机 e2e 验证记录(WSL 发行版,2026-10-24)

> 归档（2026-10-30）：一次性验证记录（结论：产品实现未发现缺陷）。三种环境形态与
> mirrored 网络替代拓扑已并入 `../runbooks/ssh-e2e.md`，复现脚本转正于 `scripts/`；
> 本轮 docker 形态 9/9 通过即 SOCKS5 与运行中增删 e2e 的验证证据（todo.md 原对应的
> unverified 项已销）。
>
> 文中的具体机器路径与口令已脱敏：口令改为起环境时随机生成（见
> `../runbooks/ssh-e2e.md` §1），仓库里不留明文口令。

在 Windows 宿主 + 全新 WSL 发行版(Ubuntu 26.04,**mirrored 网络模式**、root 用户)上,对 SSH 端口转发与跳板机做的一轮完整 e2e 验证。结论:实现未发现缺陷;发现 1 个测试代码 bug、1 个测试健壮性问题、1 处 runbook 滞后,三处均已修复并复验通过(见文末「修复记录」)。

## 测试环境

runbook(`docs/runbooks/ssh-e2e.md`)的 docker 方案在这台机器上无法复刻:mirrored 模式下 Windows 与 WSL 共享网络栈,22 端口被宿主 `dllhost.exe` 占据(经它转发到宿主的 sshd,该转发链建连后不回数据)。替代拓扑,等价复刻 docker 的端口映射语义:

| 端口 | 角色 | 配置要点 |
|---|---|---|
| 2222 | 公钥跳板(`ZHIMATERM_SSH_*`) | 临时 ed25519 密钥对、`GatewayPorts yes`(让 -R 的远端监听可从 Windows 侧连) |
| 22/2223 | 密码目标(`ZHIMATERM_SSH_PASSWORD_*`) | 密码 + keyboard-interactive(PAM)、root 与脚本生成的随机口令;监听 2223 对外,从目标机视角 `127.0.0.1:22` 取 banner 的用例依赖此映射语义 |

复现脚本已转正受版本管理,三种环境形态(`scripts/`):

| 脚本 | 形态 |
|---|---|
| `scripts/ssh-e2e-setup-host.sh` | Linux/macOS 宿主带 docker;也适用 WSL 里装 docker(WSL 内执行,Windows 侧直连 127.0.0.1) |
| `scripts/ssh-e2e-setup-wsl.sh` | Windows + WSL 无 docker(mirrored 网络)的降级拓扑 |
| `scripts/ssh-e2e-marker-service.py` | 10.99.99.1 判别地址补强验证的标记服务(方法见下文) |

测试私钥由 `scripts/ssh-e2e-setup-host.sh` 生成(`$D/client_ed25519`,Windows 侧引用,libssh2 不校验文件权限,与 Windows ssh.exe 的 ACL 要求无关)。mirrored 模式下 Windows 侧直接连 `127.0.0.1:2222/2223` 可达,无需额外转发。

## 测试清单与结果

| 用例 | 验证点 | 结果 |
|---|---|---|
| `jump_host_session_reaches_the_target_shell` | 跳板全链路:公钥跳板 → direct-tcpip → 密码目标 shell;两次主机信任按「先跳板后目标」顺序 | ✅ |
| `jump_session_carries_local_forward_rules` | 跳板 × 本地转发(`ssh -J … -L` 语义) | ⚠️ 挂死,环境原因;行为内核由下方补强验证覆盖 |
| 补强验证(临时用例,已删除) | 跳板 + 本地转发 target `10.99.99.1:2468`——该地址仅在 WSL 发行版内可达(Windows 侧实测 `TcpTestSucceeded: False`),拿到标记串即严格证明 direct-tcpip 由**目标机**发起,证据强于原用例 | ✅ 0.54s |
| `local_forward_carries_the_remote_banner` | -L 经转发端口取 sshd banner | ✅ |
| `remote_forward_carries_the_payload_back` | -R:sshd 远端监听,数据经 SSH 往返回本地 echo 服务 | ✅ |
| `socks_forward_carries_the_remote_banner` | -D:SOCKS5 方法协商/CONNECT/banner,应答先于数据 | ✅ |
| `forwards_can_be_added_and_removed_while_running` | 运行中增删:立即监听、承载流量;删除后端口立即不可连,会话不受影响 | ✅ |
| `ssh_session_runs_a_remote_shell` | 指纹确认 → 公钥认证 → 远端命令 → resize 传递 → 二次连接免确认 | ✅ |
| `recorded_host_key_change_asks_and_replaces_on_accept` / `…_aborts_when_declined` | 密钥变化的确认/中止路径 | ✅ ×2 |
| `password_authentication_reaches_a_remote_shell` | 密码路径(sshd 日志实测走 `Accepted keyboard-interactive/pam`) | ✅ |
| `shell_session_delivers_client_lang` / `lang_status_reports_rejection_without_acceptenv` | LANG 下发的接受与拒绝上报 | ✅ ×2 |
| `stacked_credentials_fall_through_to_the_key` / `stacked_failures_report_each_credential` | 素材叠加成功/逐项报错 | ✅ ×2 |
| `sftp_*`(5 个) | 目录操作、上传下载往返、树上传下载、删树、8MB 上传取消 | ✅ ×5 |
| `shell_exit_closes_the_session_and_sftp` / `dropping_the_sftp_handle_keeps_the_shell_alive` | shell 退出与会话/SFTP 生命周期 | ✅ ×2 |
| `agent_forward_lets_the_remote_side_list_local_keys` | agent 转发 | 按设计跳过(Windows 无 `SSH_AUTH_SOCK`) |
| `wrong_password_lists_every_attempted_method` | 错误口令逐项报原因 | ❌ **测试代码 bug**,见下 |

`jump_session_carries_local_forward_rules` 挂死定位:连接状态显示 zhimaterm 链路全部建立成功(跳板认证、桥接、目标认证、转发监听、目标机发起的 direct-tcpip 连接 ESTAB),数据断在 Windows 本机 22 端口——`dllhost` 转发链建连后不回数据,用例的 `tunnel.read` 无超时永久阻塞。属环境问题而非实现缺陷。

## 发现的 bug(均已修复,见文末「修复记录」)

1. **测试代码 bug(任何环境必失败)**:`wrong_password_lists_every_attempted_method` 调用 `trust_and_start`,但该 helper(tests/support.rs)收到 `ControlEvent::Failed` 即 `panic!`,而本用例密码必然错误、必然产生 `Failed`——用例自己的断言(消息含「键盘交互」+「密码」)永远执行不到。成因:重构提交 `7f6136f` 把旧 helper `trust_host`(不因认证失败 panic)统一替换为 `trust_and_start` 时机械替换了调用点;门控用例平时无人执行,故未暴露。
2. **测试健壮性**:`local_forward_carries_the_remote_banner` 与 `jump_session_carries_local_forward_rules` 的 `tunnel.read` 未设 `read_timeout`(`remote_forward` 用例设了 10s 可对照),对端不回数据会挂死整个测试进程而非超时失败。
3. **runbook 滞后**:`docs/runbooks/ssh-e2e.md` 用例表中 `shell_exit_keeps_sftp_usable_until_the_session_closes` 已更名为 `shell_exit_closes_the_session_and_sftp`(语义也变了:shell 退出即关闭会话与 SFTP),且缺 `dropping_the_sftp_handle_keeps_the_shell_alive` 一行;docker 方案在 mirrored 网络 + 宿主占用 22 的机器上不可复刻。

产品实现(SSH 转发、跳板机桥接、认证叠加)在本轮验证中**未发现缺陷**。

## 环境资产与复跑

WSL 发行版当前为 docker 形态(host 脚本起的 2222 + 容器 2223),与 WSL 直连形态的
差异只在密码目标侧用户:docker 容器是 `tester`,WSL 直连是 `root`。环境刻意保留
(与 runbook 口径一致);重搭用对应脚本,彻底重置对应发行版(`wsl --unregister <发行版名>`)。

```cmd
cd /d <项目目录>
set ZHIMATERM_SSH_HOST=127.0.0.1
set ZHIMATERM_SSH_PORT=2222
set ZHIMATERM_SSH_USER=root
set ZHIMATERM_SSH_KEY=<私钥的 Windows 路径>
set ZHIMATERM_SSH_PASSWORD_PORT=2223
set ZHIMATERM_SSH_PASSWORD_USER=tester   & :: WSL 直连形态改为 root
set ZHIMATERM_SSH_PASSWORD=<密码>
cargo test --bin zhimaterm "tests::session_e2e::ssh::<用例名>" -- --ignored --test-threads=1
```

注意:WSL 直连形态下 `jump_session_carries_local_forward_rules` 会明确失败(宿主
22 端口 banner 依赖 dllhost 转发链,环境问题,非实现缺陷);docker 形态全部通过。
10.99.99.1 补强验证:WSL 发行版内 `ip addr add 10.99.99.1/32 dev lo`、起
`scripts/ssh-e2e-marker-service.py`,临时用例经跳板转发断言标记串(样例见上文
「测试清单」补强验证行,断言 `SECRET-FROM-TESTER-10-99-99-1`)。

## 修复记录(2026-10-24,同轮修复并复验)

| 修复 | 内容 | 复验 |
|---|---|---|
| wrong_password 用例 | 去掉 `trust_and_start` 调用,用例内循环只处理 `TrustRequired`,`Failed` 留给自身断言(注释说明不能用 helper 的原因) | ✅ 通过(6.55s,失败消息含「SSH 认证未通过」+「键盘交互」+「密码」) |
| 转发用例读超时 | 两个用例的 `tunnel` 在 connect 后补 `set_read_timeout(10s)`,与 `remote_forward` 对齐 | ✅ local_forward 回归通过;jump_session 从无限挂死变为 0.55s 明确 FAILED(banner 断言) |
| runbook | 用例表更名并补 `dropping_the_sftp_handle_keeps_the_shell_alive`;开头补 Windows/mirrored 替代拓扑指引 | 文档核对 |

## docker 形态复测(2026-10-24,WSL 发行版内装 docker)

在 WSL 发行版内安装 docker.io(WSL 无 systemd,手动 `dockerd` 起守护)、执行
`scripts/ssh-e2e-setup-host.sh`(2222 宿主 sshd + 2223 debian 容器),Windows 侧
连 `127.0.0.1` 复测。**9/9 通过**,包括 WSL 直连形态下失败的
`jump_session_carries_local_forward_rules`——容器内 `127.0.0.1:22` 就是容器自己的
sshd,用例的原生判别力(证明 direct-tcpip 由目标机发起)生效,闭环确认了该用例
在 WSL 形态的失败纯属宿主 22 端口环境问题。

| 用例 | 结果 |
|---|---|
| `jump_host_session_reaches_the_target_shell` | ✅ |
| `jump_session_carries_local_forward_rules` | ✅ |
| `local_forward` / `remote_forward` / `socks_forward` / `forwards_can_be_added_and_removed` | ✅ ×4 |
| `password_authentication` / `lang_status` / `shell_session_delivers_client_lang` | ✅ ×3 |

host 脚本因此轮复测做的两处跨形态适配(均有注释说明):root 环境放行公钥登录
(`PermitRootLogin prohibit-password`,否则 WSL 内以 root 跑时自检与用例被拒);
`GatewayPorts yes`(-R 用例的远端监听绑全部接口,否则镜像回环不转发动态出现的
WSL loopback 监听,Windows 侧 ConnectionRefused;宿主直跑无副作用)。

## 未验证项

- Linux/macOS 宿主直跑形态(host 脚本在该形态只有逻辑审查,无实机;脚本对
  sftp-server 路径做了多发行版探测)。
- GUI 手动验收路径(runbook §3,含跳板机表单操作),需人工执行。
- agent 转发真实链路(需 Windows 侧可用 `SSH_AUTH_SOCK`)。
