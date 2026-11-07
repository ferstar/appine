# Login for an appine terminal tab (the android/setup map): runs as root, and $1 is the login
# account from Settings: auto, root or an account name. auto uses the first regular user created
# (the one with the smallest UID among those with UID 1000–65533 and a login shell), or root when
# there is none; a named account that no longer exists is root too. $2 is the directory to start in
# (for tabs restored after the app was killed, the android/service map); when it is missing or
# cannot be entered, the account's home directory.
#
# When a regular user first becomes the login account, it is added to the admin group and the
# privilege tool is installed, as Alpine's setup-user -a does: on Alpine the wheel group and doas
# (the app bundles that package, so it installs offline too), on Debian the sudo group and sudo
# (needs the network; if it cannot be installed, it is retried at every later login). Accounts added
# to the group are recorded in /var/lib/appine/admins; if the user later takes one out of the group,
# it is not added back. With a password set, escalating asks for the password, following the
# distro's convention; without one (adduser -D) it does not ask, and the rule is rewritten at every
# login according to the password at that time.
# The install commands all read from /dev/null: what the user types before the prompt appears is
# left for the login shell.
#
# Ctrl-C does not end the login: the script catches SIGINT itself (child processes are still
# interrupted), and pressing it while sudo installs skips installing this time.
# Caught rather than ignored: an ignored SIGINT is inherited by child processes, and apt-get could
# not be stopped; the default is restored after exec
set -u
interrupted=
trap 'interrupted=1' INT
want=${1:-auto}
dir=${2:-}
user=root

# The first regular user created
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

admins=/var/lib/appine/admins
if [ -f /etc/alpine-release ]; then
    group=wheel
    tool=doas
    rules=/etc/doas.d/30-appine.conf
else
    group=sudo
    tool=sudo
    rules=/etc/sudoers.d/appine
fi
ingroup() { id -nG "$1" 2>/dev/null | tr ' ' '\n' | grep -Fqx "$group"; }

# Admins without a password (still in the group) are not asked for a password when escalating. The
# rules are rewritten at every login, root's included, according to the passwords at that time. No
# password means a shadow field that is empty or only "!" or "*" (adduser -D writes "!"); passwd -l
# puts "!" in front of a hash, and that account still has a password. Without a shadow entry, or
# without a readable /etc/shadow, an account counts as having a password
refresh_rules() {
    nopass=
    if [ -f "$admins" ]; then
        while read -r u; do
            ingroup "$u" || continue
            pw=$(awk -F: -v u="$u" '$1 == u { sub(/^!+/, "", $2); print "=" $2; exit }' /etc/shadow 2>/dev/null)
            case $pw in '=' | '=*') nopass="$nopass $u" ;; esac
        done <"$admins"
    fi
    if [ -n "$nopass" ] && [ -d "${rules%/*}" ]; then
        {
            echo "# Written by Appine at each login: administrators without a password ($admins)."
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
}

if [ "$user" = root ]; then
    refresh_rules
    # Start in the restored directory, otherwise the home directory
    home=$(field root 6 /etc/passwd)
    { [ -n "$dir" ] && cd "$dir"; } 2>/dev/null || cd "$home" 2>/dev/null || cd /
    shell=$(field root 7 /etc/passwd)
    [ -x "$shell" ] || shell=/bin/sh
    export SHELL="$shell" HOME="$home"
    exec "$shell" -l
fi

# The first time: add to the admin group and record it (if the user later takes it out of the group,
# it is not added back). Recorded only once it really is in the group, so a failure is retried at
# the next login
fresh=
if ! grep -Fqsx -- "$user" "$admins"; then
    if [ "$group" = wheel ]; then
        addgroup "$user" wheel
        mkdir -p /etc/doas.d
        grep -Fqsx 'permit persist :wheel' /etc/doas.d/20-wheel.conf ||
            echo 'permit persist :wheel' >>/etc/doas.d/20-wheel.conf
    else
        usermod -aG sudo "$user"
    fi
    if ingroup "$user"; then
        mkdir -p "${admins%/*}"
        echo "$user" >>"$admins"
        fresh=1
    else
        echo "appine: cannot add $user to the $group group; Appine tries again at the next login" >&2
    fi
fi

# Privilege tool: once installed (or already present) it is recorded, and if the user uninstalls it
# later it is not installed again; if it could not be installed (Debian had no network then), it is
# retried at every later login
installed=/var/lib/appine/$tool-installed
tried=
if [ ! -e "$installed" ] && ! command -v "$tool" >/dev/null; then
    tried=1
    if [ "$group" = wheel ]; then
        # Install offline from the package the app bundles (without reading the repository index);
        # then add it once more by name, so world records doas rather than this file and apk upgrade
        # upgrades it as usual
        apk add -q --no-network --repositories-file /dev/null /var/cache/appine/doas.apk </dev/null &&
            apk add -q --no-network --repositories-file /dev/null doas </dev/null
    else
        # Downloading the index and the package from the network takes a few minutes, so apt's
        # output (the progress) is shown as usual. Ctrl-C during the download interrupts apt-get and
        # skips installing this time; for the dpkg step apt starts a new session in a
        # pseudo-terminal of its own, so Ctrl-C cannot reach it, and apt stops only after it has
        # finished installing
        echo "appine: installing sudo so that $user can become root (Ctrl-C skips it until the next login)"
        apt-get update </dev/null &&
            DEBIAN_FRONTEND=noninteractive apt-get install -y sudo </dev/null
    fi
fi
if command -v "$tool" >/dev/null; then
    : >"$installed"
    # Say so when the user was first added to the group, or when the tool was installed only this
    # time (no network before, skipped)
    if [ -n "$fresh" ] || [ -n "$tried" ]; then
        echo "appine: $user is in the $group group and can become root with $tool"
    fi
elif [ -n "$interrupted" ]; then
    # The progress line of the interrupted apt stopped halfway
    printf '\nappine: skipped installing %s; Appine tries again at the next login\n' "$tool" >&2
elif [ ! -e "$installed" ]; then
    echo "appine: $user is in the $group group, but $tool is not installed yet; Appine tries again at the next login" >&2
fi

refresh_rules

# Run as a login shell (reads /etc/profile); su sets HOME, SHELL, USER and LOGNAME. su -l is not
# used: it always goes to the home directory. The restored directory is entered as this account (a
# directory root can enter is not necessarily enterable by it); when it cannot be entered, go to the
# home directory
cd /
q=$(printf '%s' "$dir" | sed "s/'/'\\\\''/g")
cmd="{ [ -n '$q' ] && cd '$q'; } 2>/dev/null || cd 2>/dev/null; exec \"\$SHELL\" -l"
# util-linux's su (Debian) starts a new session for the -c command (to stop TIOCSTI from pushing
# input into root's terminal): the shell has no controlling terminal and no job control, and on
# Ctrl-C su ends the whole session, closing the tab. Its --session-command starts no new session;
# here su is the session leader, and when it exits the session ends too, so no process goes on
# reading the terminal as root. busybox su (Alpine) never starts a new session and has no such
# option
if su --help 2>&1 | grep -q -- --session-command; then
    exec su --session-command="$cmd" "$user"
fi
exec su "$user" -c "$cmd"
