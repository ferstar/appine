# appine 终端标签页的登录（设计文档 §11.1）：以 root 运行，$1 是设置里的登录账号：auto、root
# 或账号名。auto 用最先建的普通用户（UID 1000–65533、有登录 shell 的里 UID 最小的），没有就是
# root；指定的账号不在了也是 root。
#
# 普通用户第一次成为登录账号时，照 Alpine 的 setup-user -a 把它加进管理员组、装好提权工具：
# Alpine 是 wheel 组和 doas（App 带着这个包，离线也能装），Debian 是 sudo 组和 sudo（要联网，
# 装不上时之后每次登录再试）。加过组的记在 /var/lib/appine/admins，之后用户自己把它移出组也不再
# 加回去。设了密码的照发行版的
# 惯例提权时要输密码；没设密码的（adduser -D）不问，每次登录按当时的密码重新写这条规则。
# 装软件的命令都从 /dev/null 读：用户在提示符出来之前敲的字要留给登录 shell
set -u
want=${1:-auto}
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
    shell=$(field root 7 /etc/passwd)
    [ -x "$shell" ] || shell=/bin/sh
    export SHELL="$shell"
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
if [ ! -e "$installed" ] && ! command -v "$tool" >/dev/null; then
    if [ "$group" = wheel ]; then
        # 用 App 带的包离线装（不读仓库索引）；再按名字加一次，world 里记的是 doas 而不是这个
        # 文件，之后 apk upgrade 照常升级它
        apk add -q --no-network --repositories-file /dev/null /var/cache/appine/doas.apk </dev/null &&
            apk add -q --no-network --repositories-file /dev/null doas </dev/null
    else
        echo "appine: installing sudo..."
        apt-get update -qq </dev/null &&
            DEBIAN_FRONTEND=noninteractive apt-get install -y -qq sudo </dev/null >/dev/null
    fi
fi
if command -v "$tool" >/dev/null; then
    : >"$installed"
    [ -n "$fresh" ] && echo "appine: $user is in the $group group and can become root with $tool"
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

exec su -l "$user"
