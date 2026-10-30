#!/usr/bin/env bash
# SSH e2e 环境(Linux/macOS 宿主):2222 宿主 sshd(公钥)+ 2223 docker 容器(密码)。
# 与 docs/runbooks/ssh-e2e.md 配套;测完的清理步骤见 runbook §5。
set -euo pipefail

D=/tmp/zhimaterm-sshd
mkdir -p "$D" /run/sshd 2>/dev/null || true

# --- 2222:公钥服务端,宿主当前用户直跑 ---
[ -f "$D/host_ed25519" ]   || ssh-keygen -q -t ed25519 -f "$D/host_ed25519"   -N ''
[ -f "$D/client_ed25519" ] || ssh-keygen -q -t ed25519 -f "$D/client_ed25519" -N ''
cp -f "$D/client_ed25519.pub" "$D/authorized_keys"

# 密码服务的口令每次起环境时随机生成,不写死在仓库里(仓库里出现明文口令
# 会被 secret scanner 当成泄漏)。口令落在 $D/password(600),只用于回环上
# 的一次性测试服务端,清理见 runbook §4。
PASS=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
printf '%s' "$PASS" > "$D/password"
chmod 600 "$D/password"

# sftp-server 路径随发行版不同(Debian/Ubuntu、Arch、macOS 各一处)
SFTP=$(ls /usr/lib/openssh/sftp-server /usr/lib/ssh/sftp-server /usr/libexec/sftp-server /usr/libexec/openssh/sftp-server 2>/dev/null | head -1 || true)
[ -n "$SFTP" ] || { echo "找不到 sftp-server,请把它加进本脚本的探测列表" >&2; exit 1; }

# root 环境(WSL 等)当前用户就是 root,公钥登录要放行;普通用户宿主保持 no
ROOT_LOGIN=$([ "$(id -u)" = 0 ] && echo prohibit-password || echo no)
# GatewayPorts 让 -R 用例的远端监听绑全部接口:WSL 形态下测试进程在 Windows
# 宿主上,镜像回环不转发运行中动态出现的 WSL loopback 监听,绑回环连不上

cat > "$D/sshd_config" <<EOF
Port 2222
ListenAddress 127.0.0.1
HostKey $D/host_ed25519
AuthorizedKeysFile $D/authorized_keys
PidFile $D/sshd.pid
Subsystem sftp $SFTP
UsePAM no
PasswordAuthentication no
PermitRootLogin $ROOT_LOGIN
StrictModes no
AllowTcpForwarding yes
GatewayPorts yes
EOF
chmod 600 "$D/host_ed25519" "$D/authorized_keys"
[ -f "$D/sshd.pid" ] && kill "$(cat "$D/sshd.pid")" 2>/dev/null || true
sleep 1
nohup /usr/sbin/sshd -f "$D/sshd_config" -D -e >> "$D/sshd.log" 2>&1 &

# --- 2223:密码与键盘交互容器(不用 --platform:跟随本机架构,避免模拟跑 apt)---
docker rm -f zhimaterm-sshd-pw 2>/dev/null || true
docker run -d --name zhimaterm-sshd-pw \
  -p 127.0.0.1:2223:22 -e E2E_PASSWORD="$PASS" debian:stable-slim bash -c '
set -e
apt-get -qq update && apt-get -qq install -y openssh-server
mkdir -p /run/sshd
useradd -m -s /bin/bash tester && echo "tester:$E2E_PASSWORD" | chpasswd
ssh-keygen -q -A
/usr/sbin/sshd -D -e'

# 宿主端口是 docker 代理在监听,容器里 sshd 可能还在装包:等它真就绪
until docker logs zhimaterm-sshd-pw 2>&1 | grep -q "Server listening"; do sleep 5; done

# kbd-interactive 必须写进 include 目录:sshd_config 是「先出现的值生效」,
# Debian 默认配置的 Include 行早于文件尾的 KbdInteractiveAuthentication no
docker exec zhimaterm-sshd-pw sh -c \
  'printf "KbdInteractiveAuthentication yes\nPasswordAuthentication yes\n" > /etc/ssh/sshd_config.d/00-zhimaterm-test.conf
   pkill -HUP sshd'
docker exec zhimaterm-sshd-pw /usr/sbin/sshd -T | grep -iE '^(kbdinteractive|passwordauthentication)'

# --- 自检 ---
ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -i "$D/client_ed25519" "$(whoami)"@127.0.0.1 'echo KEY_AUTH_OK'

cat <<EOF

环境就绪。跑门控用例(口令从 $D/password 读,不写死在仓库里):

ZHIMATERM_SSH_HOST=127.0.0.1 \\
ZHIMATERM_SSH_PORT=2222 ZHIMATERM_SSH_USER="$(whoami)" ZHIMATERM_SSH_KEY=$D/client_ed25519 \\
ZHIMATERM_SSH_PASSWORD_PORT=2223 ZHIMATERM_SSH_PASSWORD_USER=tester \\
ZHIMATERM_SSH_PASSWORD="\$(cat $D/password)" \\
  cargo test --bin zhimaterm "tests::session_e2e::ssh" -- --ignored --nocapture --test-threads=1
EOF
