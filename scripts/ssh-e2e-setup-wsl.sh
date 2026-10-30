#!/usr/bin/env bash
# SSH e2e 环境(Windows 宿主 + WSL,mirrored 网络):docker 方案在这类机器上
# 跑不了(宿主 22 被 dllhost 占用、镜像回环与 Linux 侧竞争),用 WSL 内两台
# sshd 等价替代。适配细节与验证记录见 docs/context/ssh-e2e-wsl-tester-verification.md。
# 在 WSL 发行版内以 root 执行:bash /mnt/<盘>/.../scripts/ssh-e2e-setup-wsl.sh
set -euo pipefail

D=/tmp/zhimaterm-sshd
mkdir -p "$D" /run/sshd

# 2222 必须空闲(全新 WSL 不应有 sshd 在跑,防御性检查)
if ss -ltn 'sport = :2222' | grep -q LISTEN; then
  echo "PORT 2222 already in use:" >&2
  ss -ltnp 'sport = :2222' >&2
  exit 1
fi

[ -f "$D/host_ed25519" ]    || ssh-keygen -q -t ed25519 -f "$D/host_ed25519"    -N ''
[ -f "$D/client_ed25519" ]  || ssh-keygen -q -t ed25519 -f "$D/client_ed25519"  -N ''
[ -f "$D/host_pw_ed25519" ] || ssh-keygen -q -t ed25519 -f "$D/host_pw_ed25519" -N ''
cp -f "$D/client_ed25519.pub" "$D/authorized_keys"

# 2222:公钥跳板。GatewayPorts 让 -R 用例的远端监听(默认绑回环)可从
# Windows 宿主侧连——mirrored 下 WSL 回环与宿主回环互通,绑回环也能到,
# 这里显式打开保持与宿主监听一致的行为
cat > "$D/sshd_config" <<EOF
Port 2222
ListenAddress 0.0.0.0
HostKey $D/host_ed25519
AuthorizedKeysFile $D/authorized_keys
PidFile $D/sshd.pid
Subsystem sftp /usr/lib/openssh/sftp-server
UsePAM no
PasswordAuthentication no
PermitRootLogin prohibit-password
StrictModes no
AllowTcpForwarding yes
GatewayPorts yes
EOF

# 密码目标,对应 docker 方案的容器那台:对外 2223(容器映射语义),同时
# 尝试占住 WSL 侧 127.0.0.1:22——跳板转发用例从目标机视角连 127.0.0.1:22
# 取 banner。mirrored 下该绑定多半因宿主已占用而失败(sshd 仅告警),此时
# 22 由宿主服务应答,用例依赖的 banner 仍可拿到
# 口令每次起环境时随机生成,不写死在仓库里(仓库里出现明文口令会被
# secret scanner 当成泄漏)。落在 $D/password(600),只用于回环上的一次性
# 测试服务端,清理见 runbook §4。
PASS=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
printf '%s' "$PASS" > "$D/password"
chmod 600 "$D/password"
echo "root:$PASS" | chpasswd
cat > "$D/sshd_pw_config" <<EOF
ListenAddress 0.0.0.0:2223
ListenAddress 127.0.0.1:22
HostKey $D/host_pw_ed25519
PidFile $D/sshd_pw.pid
Subsystem sftp /usr/lib/openssh/sftp-server
UsePAM yes
PasswordAuthentication yes
KbdInteractiveAuthentication yes
PermitRootLogin yes
StrictModes no
AllowTcpForwarding yes
GatewayPorts yes
AcceptEnv LANG LC_*
EOF

chmod 600 "$D/host_ed25519" "$D/host_pw_ed25519" "$D/authorized_keys"
[ -f "$D/sshd.pid" ]    && kill "$(cat "$D/sshd.pid")"    2>/dev/null || true
[ -f "$D/sshd_pw.pid" ] && kill "$(cat "$D/sshd_pw.pid")" 2>/dev/null || true
sleep 1

nohup /usr/sbin/sshd -f "$D/sshd_config"    -D -e >> "$D/sshd.log"    2>&1 &
nohup /usr/sbin/sshd -f "$D/sshd_pw_config" -D -e >> "$D/sshd_pw.log" 2>&1 &

for i in $(seq 1 20); do
  if ss -ltn | grep -q ':2222 ' && ss -ltn | grep -q ':2223 '; then break; fi
  sleep 0.5
done
ss -ltn | grep -E ':(22|2223|2222) ' || true

# 自检:公钥通道跑远端命令;密码通道至少 banner 可读(密码路径由 cargo 用例验证)
ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -i "$D/client_ed25519" root@127.0.0.1 'echo KEY_AUTH_OK'
timeout 5 bash -c 'exec 3<>/dev/tcp/127.0.0.1/2223; head -c 16 <&3' && echo

cat <<EOF

WSL 侧就绪。Windows 侧引用私钥(\$D/client_ed25519,UNC 路径
\\\\wsl.localhost\\<distro>\\tmp\\zhimaterm-sshd\\client_ed25519 或复制到本地)
后跑门控用例(cmd):

set ZHIMATERM_SSH_HOST=127.0.0.1
set ZHIMATERM_SSH_PORT=2222
set ZHIMATERM_SSH_USER=root
set ZHIMATERM_SSH_KEY=<私钥的 Windows 路径>
set ZHIMATERM_SSH_PASSWORD_PORT=2223
set ZHIMATERM_SSH_PASSWORD_USER=root
set ZHIMATERM_SSH_PASSWORD=<WSL 侧 $D/password 里的口令,Windows 侧读 \\\\wsl.localhost\\<distro>\\tmp\\zhimaterm-sshd\\password>
cargo test --bin zhimaterm "tests::session_e2e::ssh::<用例名>" -- --ignored --test-threads=1

注意:jump_session_carries_local_forward_rules 依赖 127.0.0.1:22 的 banner,
在宿主 22 被占用且其转发链不回数据的机器上会明确失败(环境限制,非实现缺陷)。
EOF
