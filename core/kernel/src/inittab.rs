//! 内置 init 执行 `/etc/inittab` 的一个子集（设计文档 §6）：开机时的 `sysinit`、`boot`、
//! `bootwait`、`wait`、`once`，关机时的 `shutdown`（busybox 的格式）和 0、6 运行级别的
//! `wait`、`once`（sysvinit 的格式）。Alpine 的 inittab 由这几步运行 OpenRC，Debian 的
//! sysvinit-core 运行 `/etc/init.d/rc`。没有终端，`respawn`、`askfirst`（getty）不运行：
//! 登录 shell 是 App 的标签页。
//!
//! 每一项由 `/bin/sh -c` 执行（busybox init 遇到 shell 的特殊字符时也这样做），stdin 是
//! `/dev/null`，stdout、stderr 写到 [`LOG`]：appine 没有控制台。

/// 开机、关机时 inittab 里的命令的输出。每次开机时清空。
pub const LOG: &str = "/var/log/boot.log";

/// inittab 里的命令的环境变量，与 busybox init 设的相同。
pub(crate) const ENV: [&[u8]; 5] = [
    b"HOME=/",
    b"PATH=/sbin:/usr/sbin:/bin:/usr/bin",
    b"SHELL=/bin/sh",
    b"USER=root",
    b"TERM=linux",
];

/// 关机的方式：busybox 的 reboot、halt、poweroff 不带 `-f` 时给 1 号进程发 SIGTERM、
/// SIGUSR1、SIGUSR2，由 init 运行关机的命令、结束所有进程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Reboot,
    Halt,
    PowerOff,
}

impl Power {
    /// busybox init 的信号处理：SIGTERM 重启，SIGUSR1 停机，SIGUSR2 关机。
    pub(crate) fn from_signal(sig: crate::process::Signal) -> Option<Power> {
        use crate::process::Signal;
        match sig {
            Signal::TERM => Some(Power::Reboot),
            Signal::USR1 => Some(Power::Halt),
            Signal::USR2 => Some(Power::PowerOff),
            _ => None,
        }
    }

    /// sysvinit 的运行级别：停机、关机是 0，重启是 6。
    fn runlevel(self) -> u8 {
        match self {
            Power::Reboot => b'6',
            Power::Halt | Power::PowerOff => b'0',
        }
    }
}

/// inittab 的一项：`id:runlevels:action:process`。busybox 的格式里第一列是终端、第二列
/// 总是空的；sysvinit 的格式第二列是运行级别，空的表示所有运行级别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub runlevels: Vec<u8>,
    pub action: Vec<u8>,
    pub process: Vec<u8>,
}

impl Entry {
    fn is(&self, action: &[u8]) -> bool {
        self.action == action
    }

    /// 属于运行级别 `level`：没写运行级别的属于所有运行级别。
    fn in_level(&self, level: u8) -> bool {
        self.runlevels.is_empty() || self.runlevels.contains(&level)
    }
}

/// 解析 inittab：`#` 开头的行和空行跳过，少于四列的跳过；第四列（命令）可以含 `:`。
pub(crate) fn parse(text: &[u8]) -> Vec<Entry> {
    text.split(|&b| b == b'\n')
        .filter_map(|line| {
            let line = line.trim_ascii();
            if line.is_empty() || line[0] == b'#' {
                return None;
            }
            let mut fields = line.splitn(4, |&b| b == b':');
            let _id = fields.next()?;
            let runlevels = fields.next()?.trim_ascii().to_vec();
            let action = fields.next()?.trim_ascii().to_vec();
            let process = fields.next()?.trim_ascii().to_vec();
            Some(Entry {
                runlevels,
                action,
                process,
            })
        })
        .collect()
}

/// 开机时依次运行的命令和要不要等它结束：先是 `sysinit`，然后 `boot`、`bootwait`，
/// 然后默认运行级别（`initdefault` 的，没有时是 2）的 `wait`，最后是它的 `once`（与
/// busybox init 的顺序相同：所有 `wait` 在所有 `once` 之前）。
pub(crate) fn boot_plan(entries: &[Entry]) -> Vec<(Vec<u8>, bool)> {
    let level = entries
        .iter()
        .find(|e| e.is(b"initdefault"))
        .and_then(|e| e.runlevels.first().copied())
        .unwrap_or(b'2');
    // 每一步：这一项属于它时，要不要等它结束
    type Phase<'a> = &'a dyn Fn(&Entry) -> Option<bool>;
    let phases: [Phase; 4] = [
        &|e| e.is(b"sysinit").then_some(true),
        &|e| (e.is(b"boot") || e.is(b"bootwait")).then_some(e.is(b"bootwait")),
        &|e| (e.is(b"wait") && e.in_level(level)).then_some(true),
        &|e| (e.is(b"once") && e.in_level(level)).then_some(false),
    ];
    let mut plan = Vec::new();
    for phase in phases {
        for e in entries.iter().filter(|e| !e.process.is_empty()) {
            if let Some(wait) = phase(e) {
                plan.push((e.process.clone(), wait));
            }
        }
    }
    plan
}

/// 关机时依次运行的命令（都等它结束）：busybox 格式的 `shutdown`，以及 sysvinit 格式里
/// 写明了运行级别 0（停机、关机）或者 6（重启）的 `wait`、`once`。
pub(crate) fn shutdown_plan(entries: &[Entry], power: Power) -> Vec<Vec<u8>> {
    let level = power.runlevel();
    entries
        .iter()
        .filter(|e| !e.process.is_empty())
        .filter(|e| {
            e.is(b"shutdown")
                || ((e.is(b"wait") || e.is(b"once"))
                    && !e.runlevels.is_empty()
                    && e.runlevels.contains(&level))
        })
        .map(|e| e.process.clone())
        .collect()
}

/// 命令的程序（第一个词）：绝对路径时返回它，用来跳过没有安装的（Alpine 的 inittab 里有
/// `/sbin/openrc`，没装 OpenRC 时不必每次开机都起一个 shell 报错）。
pub(crate) fn program(process: &[u8]) -> Option<&[u8]> {
    let word = process.split(|b| b.is_ascii_whitespace()).next()?;
    word.starts_with(b"/").then_some(word)
}

/// 由 `/bin/sh -c` 执行的脚本：stdin 是 `/dev/null`，输出接到 [`LOG`]（`first` 时先清空），
/// 再 `exec` 命令。`/var/log` 不在时输出丢掉。
pub(crate) fn script(process: &[u8], first: bool) -> Vec<u8> {
    let redirect = if first { ">" } else { ">>" };
    let mut s = format!(
        "exec </dev/null; if [ -d /var/log ]; then exec {redirect}{LOG} 2>&1; \
         else exec >/dev/null 2>&1; fi; exec "
    )
    .into_bytes();
    s.extend_from_slice(process);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPINE: &[u8] = b"# /etc/inittab

::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default

# Set up a couple of getty's
tty1::respawn:/sbin/getty 38400 tty1
tty2::respawn:/sbin/getty 38400 tty2

::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
";

    const DEBIAN: &[u8] = b"id:2:initdefault:
si::sysinit:/etc/init.d/rcS
~:S:wait:/sbin/sulogin --force
l0:0:wait:/etc/init.d/rc 0
l1:1:wait:/etc/init.d/rc 1
l2:2:wait:/etc/init.d/rc 2
l6:6:wait:/etc/init.d/rc 6
z6:6:respawn:/sbin/sulogin --force
ca:12345:ctrlaltdel:/sbin/shutdown -t1 -a -r now
1:2345:respawn:/sbin/getty --noclear 38400 tty1
";

    fn plan(text: &[u8]) -> Vec<(String, bool)> {
        boot_plan(&parse(text))
            .into_iter()
            .map(|(p, w)| (String::from_utf8(p).unwrap(), w))
            .collect()
    }

    fn down(text: &[u8], power: Power) -> Vec<String> {
        shutdown_plan(&parse(text), power)
            .into_iter()
            .map(|p| String::from_utf8(p).unwrap())
            .collect()
    }

    #[test]
    fn alpine_runs_openrc() {
        assert_eq!(
            plan(ALPINE),
            [
                ("/sbin/openrc sysinit".to_owned(), true),
                ("/sbin/openrc boot".to_owned(), true),
                ("/sbin/openrc default".to_owned(), true),
            ]
        );
        for power in [Power::Reboot, Power::Halt, Power::PowerOff] {
            assert_eq!(down(ALPINE, power), ["/sbin/openrc shutdown"]);
        }
    }

    #[test]
    fn sysvinit_follows_runlevels() {
        assert_eq!(
            plan(DEBIAN),
            [
                ("/etc/init.d/rcS".to_owned(), true),
                ("/etc/init.d/rc 2".to_owned(), true),
            ]
        );
        assert_eq!(down(DEBIAN, Power::PowerOff), ["/etc/init.d/rc 0"]);
        assert_eq!(down(DEBIAN, Power::Reboot), ["/etc/init.d/rc 6"]);
    }

    #[test]
    fn order_and_parsing() {
        let text = b"a::once:/bin/late
b::wait:/bin/first wait
c:3:wait:/bin/other level
  # comment
d::bootwait:/bin/bw
e::boot:/bin/b
f::sysinit:/bin/s: with a colon
g::wait
h::sysinit:
";
        assert_eq!(
            plan(text),
            [
                ("/bin/s: with a colon".to_owned(), true),
                ("/bin/bw".to_owned(), true),
                ("/bin/b".to_owned(), false),
                ("/bin/first wait".to_owned(), true),
                ("/bin/late".to_owned(), false),
            ]
        );
        assert_eq!(program(b"/sbin/openrc default"), Some(&b"/sbin/openrc"[..]));
        assert_eq!(program(b"echo hi"), None);
    }
}
