# Debian（glibc）：sshd 的登录会话里 w、who 报告的空闲时间（终端的访问时间，第 79 轮）。
# Alpine 的 musl 不写 utmp，w、who 什么也不列，所以用 Debian。会话先闲 20 秒再看
export DEBIAN_FRONTEND=noninteractive
[ -s /etc/resolv.conf ] || echo 'nameserver 127.0.0.53' > /etc/resolv.conf
if ! [ -x /usr/sbin/sshd ] || ! command -v w >/dev/null; then
  apt-get update -q >/dev/null 2>&1
  apt-get install -y -q --no-install-recommends openssh-server openssh-client procps >/tmp/apt-ssh.log 2>&1
  echo "apt-get install: $?"
fi
mkdir -p /run/sshd /root/.ssh; chmod 700 /root/.ssh
ssh-keygen -A >/dev/null
[ -f /root/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/id_ed25519
cp /root/.ssh/id_ed25519.pub /root/.ssh/authorized_keys; chmod 600 /root/.ssh/authorized_keys
touch /var/run/utmp /var/log/wtmp
rm -f /tmp/sshd.log
/usr/sbin/sshd -p 2222 -E /tmp/sshd.log -o PermitRootLogin=prohibit-password
echo "sshd started: $?"
sleep 1
O="-p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o LogLevel=ERROR"
ssh $O -tt root@127.0.0.1 'tty; sleep 20; w -h; who -u' </dev/null | tr -d '\r' > /tmp/idle.out
cat /tmp/idle.out
kill $(cat /run/sshd.pid) 2>/dev/null
echo "== sshd log"; cat /tmp/sshd.log
