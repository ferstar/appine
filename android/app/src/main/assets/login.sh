# appine 终端标签页的登录（android/setup 的 map）：以 root 运行，$1 是设置里的登录账号：auto、root
# 或账号名。auto 用最先建的普通用户（UID 1000–65533、有登录 shell 的里 UID 最小的），没有就是
# root；指定的账号不在了也是 root。$2 是从哪个目录开始（App 被杀之后恢复的标签页，android/service 的 map），
# 没有、进不去时是账号的家目录。
#
# 普通用户第一次成为登录账号时，照 Alpine 的 setup-user -a 把它加进管理员组、装好提权工具：
# Alpine 是 wheel 组和 doas（App 带着这个包，离线也能装），Debian 是 sudo 组和 sudo（要联网，
# 装不上时之后每次登录再试）。加过组的记在 /var/lib/appine/admins，之后用户自己把它移出组也不再
# 加回去。设了密码的照发行版的
# 惯例提权时要输密码；没设密码的（adduser -D）不问，每次登录按当时的密码重新写这条规则。
# 装软件的命令都从 /dev/null 读：用户在提示符出来之前敲的字要留给登录 shell。
#
# Ctrl-C 不结束登录：脚本自己接住 SIGINT（子进程照样被打断），装 sudo 时按了就跳过这一次。
# 接住而不是忽略：忽略会传给子进程，apt-get 就停不下来了；exec 之后恢复默认
set -u
interrupted=
trap 'interrupted=1' INT
want=${1:-auto}
dir=${2:-}
user=root

# 最先建的普通用户
first() {
    best=
    bestuid=
    while IFS=: read -r name _ uid _ _ _ shell; do
        case $shell in '' | */nologin | */false) continue ;; esac
        case $uid in '' | *[!0-9]*) continue ;; esac
        [ "$uid" -ge 1000 ] && [ "$uid" -lt 65534 ] || continue
        if [ -z "$bestuid" ] || [ "$uid" -lt "$bestuid" ]; then
            best=$name
            bestuid=$uid
        fi
    done </etc/passwd
    echo "$best"
}

field() { awk -F: -v u="$1" -v f="$2" '$1 == u { print $f; exit }' "$3"; }

case $want in
auto)
    u=$(first)
    [ -n "$u" ] && user=$u
    ;;
root) ;;
*)
    if [ -n "$(field "$want" 1 /etc/passwd)" ]; then
        user=$want
    else
        echo "appine: there is no user $want any more, logging in as root" >&2
    fi
    ;;
esac

if [ "$user" = root ]; then
    # 从恢复的目录开始，不然是家目录
    home=$(field root 6 /etc/passwd)
    { [ -n "$dir" ] && cd "$dir"; } 2>/dev/null || cd "$home" 2>/dev/null || cd /
    shell=$(field root 7 /etc/passwd)
    [ -x "$shell" ] || shell=/bin/sh
    export SHELL="$shell" HOME="$home"
    exec "$shell" -l
fi

admins=/var/lib/appine/admins
if [ -f /etc/alpine-release ]; then
    group=wheel
    tool=doas
else
    group=sudo
    tool=sudo
fi
ingroup() { id -nG "$1" 2>/dev/null | tr ' ' '\n' | grep -qx "$group"; }

# 第一次：加进管理员组，记下来（之后用户自己移出组也不再加回去）
if ! grep -qsx "$user" "$admins"; then
    if [ "$group" = wheel ]; then
        addgroup "$user" wheel
        mkdir -p /etc/doas.d
        grep -qsx 'permit persist :wheel' /etc/doas.d/20-wheel.conf ||
            echo 'permit persist :wheel' >>/etc/doas.d/20-wheel.conf
    else
        usermod -aG sudo "$user"
    fi
    mkdir -p "${admins%/*}"
    echo "$user" >>"$admins"
    fresh=1
else
    fresh=
fi

# 提权工具：装上过一次（或者本来就有）就记下，之后用户自己卸掉也不再装；没装上的（Debian
# 当时没网）之后每次登录再试
installed=/var/lib/appine/$tool-installed
tried=
if [ ! -e "$installed" ] && ! command -v "$tool" >/dev/null; then
    tried=1
    if [ "$group" = wheel ]; then
        # 用 App 带的包离线装（不读仓库索引）；再按名字加一次，world 里记的是 doas 而不是这个
        # 文件，之后 apk upgrade 照常升级它
        apk add -q --no-network --repositories-file /dev/null /var/cache/appine/doas.apk </dev/null &&
            apk add -q --no-network --repositories-file /dev/null doas </dev/null
    else
        # 联网下载索引和包要几分钟，照常显示 apt 的输出（进度）。下载时按 Ctrl-C，apt-get 被
        # 打断、这次不装；dpkg 那一步 apt 让它在自己开的伪终端里另起会话，Ctrl-C 碰不到它，
        # apt 等它装完才停
        echo "appine: installing sudo so that $user can become root (Ctrl-C skips it until the next login)"
        apt-get update </dev/null &&
            DEBIAN_FRONTEND=noninteractive apt-get install -y sudo </dev/null
    fi
fi
if command -v "$tool" >/dev/null; then
    : >"$installed"
    # 第一次加进组时，或者这一次才装上时（之前没网、跳过了）说一声
    if [ -n "$fresh" ] || [ -n "$tried" ]; then
        echo "appine: $user is in the $group group and can become root with $tool"
    fi
elif [ -n "$interrupted" ]; then
    # 被打断的 apt 的进度行停在一半
    printf '\nappine: skipped installing %s; appine tries again at the next login\n' "$tool" >&2
elif [ ! -e "$installed" ]; then
    echo "appine: $user is in the $group group, but $tool is not installed yet; appine tries again at the next login" >&2
fi

# 没设密码的管理员（还在组里的）提权时不问密码
nopass=
while read -r u; do
    ingroup "$u" || continue
    case $(field "$u" 2 /etc/shadow) in '' | '!'* | '*'*) nopass="$nopass $u" ;; esac
done <"$admins"
if [ "$group" = wheel ]; then
    rules=/etc/doas.d/30-appine.conf
else
    rules=/etc/sudoers.d/appine
fi
if [ -n "$nopass" ] && [ -d "${rules%/*}" ]; then
    {
        echo "# Written by appine at each login: administrators without a password ($admins)."
        for u in $nopass; do
            if [ "$group" = wheel ]; then
                echo "permit nopass $u"
            else
                echo "$u ALL=(ALL:ALL) NOPASSWD: ALL"
            fi
        done
    } >"$rules"
    chmod 0440 "$rules"
else
    rm -f "$rules"
fi

# 以登录 shell 运行（读 /etc/profile）；su 设好 HOME、SHELL、USER、LOGNAME。不用 su -l：它
# 总是回家目录。恢复的目录以这个账号的身份进（root 进得去的它不一定进得去），进不去回家目录
cd /
q=$(printf '%s' "$dir" | sed "s/'/'\\\\''/g")
cmd="{ [ -n '$q' ] && cd '$q'; } 2>/dev/null || cd 2>/dev/null; exec \"\$SHELL\" -l"
# util-linux 的 su（Debian）对 -c 的命令另起会话（防 TIOCSTI 往 root 的终端里塞字）：shell
# 没有控制终端、没有作业控制，按 Ctrl-C 时 su 结束整个会话，标签页就关了。它的
# --session-command 不另起会话；这里 su 是会话组长，它退出时会话也就结束了，没有以 root 身份
# 接着读终端的进程。busybox su（Alpine）本来就不另起会话，也没有这个选项
if su --help 2>&1 | grep -q -- --session-command; then
    exec su --session-command="$cmd" "$user"
fi
exec su "$user" -c "$cmd"
