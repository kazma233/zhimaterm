# 跑 SSH 端到端验证

SSH 传输需要一个真实 sshd。这里准备两台**一次性、只监听回环**的测试服务端：

| 端口 | 形态 | 用途 | 认证 |
|---|---|---|---|
| 2222 | 当前用户直接跑 `/usr/sbin/sshd` | 公钥、主机密钥信任 | pubkey（只认临时密钥） |
| 2223 | docker 容器里的 sshd | 密码与键盘交互 | password + keyboard-interactive |

两台都刻意留着不关，方便反复跑门控用例；要清理见文末。

环境形态三选一（都实测过，见 `../archive/ssh-e2e-wsl-tester-verification.md`）：

- Linux/macOS 宿主带 docker：`bash scripts/ssh-e2e-setup-host.sh`
- Windows + WSL（WSL 里装 docker）：WSL 内跑同一条 host 脚本，Windows 侧直连 `127.0.0.1`（镜像回环互通）
- Windows + WSL 无 docker（mirrored 网络）：`bash scripts/ssh-e2e-setup-wsl.sh`，两台 sshd 等价拓扑（宿主 22 被占用、镜像回环与 Linux 侧竞争时 docker 方案跑不了的降级路径）

## 1. 起两台服务端（host 脚本）

```bash
bash scripts/ssh-e2e-setup-host.sh
```

脚本幂等（重跑先停旧实例、重建容器），末尾自检通过即就绪。配置要点：

- 2222 宿主直跑的公钥 sshd：root 环境（WSL 等）放行公钥登录（`prohibit-password`），普通用户宿主保持 `no`；`GatewayPorts yes` 是 WSL 形态必需——镜像回环不转发运行中动态出现的 loopback 监听，`-R` 用例的远端监听必须绑全部接口，宿主直跑场景无副作用。
- 2223 容器不写 `--platform`，跟随本机架构，避免模拟跑 apt。
- 密码服务的口令每次起环境时随机生成，落在 `$D/password`（600 权限），不写死在
  仓库里；重跑脚本会重新生成，旧的 `$D` 目录删掉即失效。

排障时看这里（脚本已处理，理解行为的两个知识点）：

- 宿主机端口是 docker 代理在监听，容器里 sshd 可能还在装包：要等日志出现
  `Server listening` 才算就绪。
- 开启键盘交互必须写进 include 目录：sshd_config 是"先出现的值生效"，
  Debian 默认配置里第 12 行 `Include /etc/ssh/sshd_config.d/*.conf`
  早于第 64 行的 `KbdInteractiveAuthentication no`，追加到文件末尾不生效。

## 2. 跑门控用例

```bash
D=/tmp/zhimaterm-sshd
ZHIMATERM_SSH_HOST=127.0.0.1 \
ZHIMATERM_SSH_PORT=2222 ZHIMATERM_SSH_USER="$(whoami)" ZHIMATERM_SSH_KEY=$D/client_ed25519 \
ZHIMATERM_SSH_PASSWORD_PORT=2223 ZHIMATERM_SSH_PASSWORD_USER=tester \
ZHIMATERM_SSH_PASSWORD="$(cat $D/password)" \
  cargo test --bin zhimaterm "tests::session_e2e::ssh" -- --ignored --nocapture --test-threads=1
```

密码服务的口令由起环境的脚本随机生成，落在 `$D/password`（600 权限），不写死在
仓库里——仓库里的明文口令会被 secret scanner 当成泄漏。换环境后旧的 `$D`
目录一起删（见 §4），口令即失效。

### agent 转发用例需要本地 agent

`agent_forward_lets_the_remote_side_list_local_keys` 依赖一个能连上的本地
ssh-agent（靠 `SSH_AUTH_SOCK` 发现）。临时 agent 即可，用例自己装卸测试钥匙，
不碰常驻 agent：

```bash
eval "$(ssh-agent -s)"
export SSH_AUTH_SOCK SSH_AGENT_PID
# 与上面同一组 ZHIMATERM_SSH_* 环境变量一起跑；没有 SSH_AUTH_SOCK 时该用例跳过
```

认证服务的 `AllowAgentForwarding` 保持默认（yes）即可；用例断言远端
`ssh-add -l` 能列出本机 agent 的钥匙指纹。

用例自己把 `ZHIMATERM_CONFIG_DIR` 指到临时目录，不会碰真实的主机密钥记录。

覆盖到的行为：

| 用例 | 验证点 |
|---|---|
| `ssh_session_runs_a_remote_shell` | 首次确认指纹 → 公钥认证 → 远端命令 → resize 传到远端 → 二次连接免确认 |
| `recorded_host_key_change_asks_and_replaces_on_accept` / `…_aborts_when_declined` | 指纹变化弹警示确认：「信任新密钥并连接」才替换记录并继续，取消/拒绝则中止 |
| `sftp_directory_operations_work_alongside_the_shell` | realpath/readdir/stat/mkdir/rmdir/unlink 与 shell 在同一会话上交替；rename 目标冲突报错且既有目录不被覆盖 |
| `sftp_upload_and_download_round_trip` | 300 KB 上传（rename 提交）→ 下载逐字节比对；未确认覆盖报冲突；确认覆盖完成；远端临时文件清理干净 |
| `sftp_tree_upload_and_download_round_trip` | 嵌套目录树上传（逐级 mkdir + 逐文件 rename）→ 整树下载回本地逐字节比对；空目录重建 |
| `sftp_delete_tree_removes_nested_directories` | 嵌套树 7 个条目全删（文件/链接/目录），符号链接目标保留 |
| `sftp_upload_cancel_reaches_a_terminal_state` | 8 MB 上传取消 → 取消终态，远端不留目标文件 |
| `shell_exit_closes_the_session_and_sftp` | shell 退出即结束整个会话：SFTP 一并关闭、退出归类 ShellExit 不触发自动重连、句柄立即报「已关闭」（ADR-005） |
| `dropping_the_sftp_handle_keeps_the_shell_alive` | 丢弃全部 SFTP 句柄后 shell 继续可用 |
| `password_authentication_reaches_a_remote_shell` | 「密码」项走键盘交互成功（服务端日志 `Accepted keyboard-interactive/pam`） |
| `shell_session_delivers_client_lang` | 客户端 LANG 随 shell 下发且状态事件报告已接受（ADR-019；依赖容器 sshd 的 `AcceptEnv LANG LC_*`，Debian 默认配置自带） |
| `lang_status_reports_rejection_without_acceptenv` | 未配 AcceptEnv 的服务器拒绝 env 请求：状态事件送达且 applied=false，连接照常（ADR-019） |
| `jump_host_session_reaches_the_target_shell` | 跳板机全链路（ADR-020）：2222 公钥当跳板 → direct-tcpip → 2223 密码认证目标 shell 可用；两次主机信任按「先跳板后目标」顺序 |
| `jump_session_carries_local_forward_rules` | 跳板 + 本地转发组合（ADR-020 × ADR-011）：转发规则挂目标会话、direct-tcpip 由目标机发起（同 `ssh -J … -L` 语义），经桥转发取回容器自身 sshd banner |
| `wrong_password_lists_every_attempted_method` | 错误口令时键盘交互与 password 各自报原因 |
| `stacked_credentials_fall_through_to_the_key` | 素材叠加：agent+私钥+密码同时提供时按顺序落到可用者（ADR-010） |
| `stacked_failures_report_each_credential` | 叠层全败时逐项报原因，不静默降级 |
| `local_forward_carries_the_remote_banner` | -L 本地转发：经转发端口取回 sshd banner（ADR-011） |
| `remote_forward_carries_the_payload_back` | -R 远程转发：sshd 远端监听，数据经 SSH 往返回本地 echo 服务 |
| `socks_forward_carries_the_remote_banner` | -D 动态 SOCKS：方法协商/CONNECT/banner，应答先于数据（ADR-038） |
| `forwards_can_be_added_and_removed_while_running` | 运行中增删转发：立即监听/承载流量，删除后端口立即不可连，会话不受影响（ADR-038） |
| `password_prompt_falls_back_when_no_static_auth` | 无静态素材的条目经连接期询问连上（ADR-018 修订） |
| `password_prompt_gives_up_after_three_wrong_answers` | 连接期询问三次全错按认证失败收尾，不再询问（ADR-018 修订） |
| `password_prompt_after_agent_keys_are_rejected` | agent 钥匙被拒后落到密码路仍询问，经键盘交互连上（ADR-018 修订补记） |

## 3. 手动验证界面

弹窗默认是已保存连接的列表（ADR-048），先进「新建连接」才有表单；认证信息
（agent / 私钥 / 密码）是叠加关系，不互斥（ADR-010）。

- 侧栏底部「SSH」→「新建连接」：主机 `127.0.0.1` / 端口 `2222` / 当前用户名，
  认证信息加私钥文件 `/tmp/zhimaterm-sshd/client_ed25519`，「保存并连接」：
  应弹主机密钥确认，确认后进入远端 shell，侧栏该会话行的第二行显示来源标签
  `user@127.0.0.1:2222`。
- 同一表单填 `127.0.0.1` / `2223` / `tester` + 密码（`cat /tmp/zhimaterm-sshd/password`），
  「保存并连接」
  应直接连上（agent 开关默认开，找不到 agent 时只是失败信息里说明，不影响密码路）。
- 复现「指纹变化」：改掉受管记录（macOS 上是
  `~/Library/Application Support/Zhimaterm/known_hosts`）里那条密钥再连一次，
  应弹警示文案更强的确认浮层（按钮「信任新密钥并连接」）：确认后替换记录并继续，
  取消则中止连接。
- 跳板机（ADR-020）：表单开「经跳板机连接」，跳板填 `127.0.0.1:2222` +
  当前用户 + 私钥 `/tmp/zhimaterm-sshd/client_ed25519`，目标填 `127.0.0.1` /
  `2223` / `tester` + 密码（`cat /tmp/zhimaterm-sshd/password`）：应先弹跳板的主机密钥确认、再弹目标的，连上后
  侧栏来源标签带「经 user@127.0.0.1:2222」。
- `COLORTERM`（ADR-032）：连上后 `echo $COLORTERM`。容器 sshd 是 Debian 默认的
  `AcceptEnv LANG LC_*`，应为空（被拒是预期，连接不受影响）；若在容器 sshd_config
  里追加 `AcceptEnv COLORTERM` 并重启 sshd，再连应为 `truecolor`。

## 4. 清理

```bash
[ -f /tmp/zhimaterm-sshd/sshd.pid ] && kill "$(cat /tmp/zhimaterm-sshd/sshd.pid)"
rm -rf /tmp/zhimaterm-sshd /tmp/zhimaterm-ssh-test-*
docker rm -f zhimaterm-sshd-pw
```

## 已知噪声

- 非 root 用户跑 sshd 时日志里会有 `Attempt to write login records by non-root user (aborting)`，
  不影响会话。
- 容器测试用户与口令（`tester` 与 `$D/password` 里的随机口令）只用于回环上的临时服务端，不要复用到任何真实环境。
