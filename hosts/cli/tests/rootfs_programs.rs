//! 端到端测试：把 Alpine minirootfs 导入 inode 仓库，经由 `ld-musl` 运行 busybox
//! （M1b、M3a、M3b 的退出标准）。每个用例都分别在 Tier 0 和 Tier 1 下运行。
//!
//! 交互式的用例用 `appine-run --pty` 在进程内 PTY 上运行 `sh -i`，经由管道发送按键
//! （^C、^Z 就是字节 0x03、0x1a），等到输出里出现预期的文字再发下一步。
//!
//! rootfs 由 `scripts/fetch-rootfs` 下载到 `target/rootfs/`，也可以用环境变量
//! `APPINE_ROOTFS_TARBALL` 指定。找不到时跳过这些测试；设置
//! `APPINE_REQUIRE_ROOTFS=1`（CI 这样做）时改为失败。
//!
//! 需要外网的用例（M4 的退出标准：apk 安装软件包、git 经由 HTTPS 克隆）只在设置了
//! `APPINE_NETWORK_TESTS=1` 时运行，宿主的 `http_proxy`、`https_proxy` 会传给 guest。
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TIERS: [&str; 2] = ["0", "1"];
const PATH: &str = "PATH=/usr/sbin:/usr/bin:/sbin:/bin";

fn find_tarball() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("APPINE_ROOTFS_TARBALL") {
        return Some(PathBuf::from(p));
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/rootfs");
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.starts_with("alpine-minirootfs-") && name.ends_with("-aarch64.tar.gz")
        })
        .collect();
    found.sort();
    found.pop()
}

/// 导入一次 rootfs，所有用例共用同一个仓库。
fn store() -> Option<&'static Path> {
    static STORE: OnceLock<Option<PathBuf>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            let Some(tarball) = find_tarball() else {
                assert!(
                    std::env::var_os("APPINE_REQUIRE_ROOTFS").is_none_or(|v| v != "1"),
                    "rootfs tarball not found; run scripts/fetch-rootfs"
                );
                eprintln!("skipping rootfs tests: run scripts/fetch-rootfs to enable them");
                return None;
            };
            let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rootfs-store");
            let _ = std::fs::remove_dir_all(&dir);
            let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
                .arg("--rootfs")
                .arg(&dir)
                .arg("--import")
                .arg(&tarball)
                .output()
                .unwrap();
            assert!(out.status.success(), "import failed: {}", stderr(&out));
            Some(dir)
        })
        .as_deref()
}

fn command(store: &Path, tier: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_appine-run"));
    cmd.args(["--tier", tier, "--env", PATH, "--rootfs"])
        .arg(store)
        .arg("--")
        .args(args);
    cmd
}

/// 在每个执行层下运行一次，返回 (执行层, 输出)；没有 rootfs 时返回空表。
fn run(args: &[&str]) -> Vec<(&'static str, Output)> {
    let Some(store) = store() else {
        return Vec::new();
    };
    TIERS
        .iter()
        .map(|&tier| (tier, command(store, tier, args).output().unwrap()))
        .collect()
}

fn stdout(out: &Output) -> &str {
    std::str::from_utf8(&out.stdout).unwrap()
}

fn stderr(out: &Output) -> &str {
    std::str::from_utf8(&out.stderr).unwrap()
}

fn assert_ok(tier: &str, out: &Output) {
    assert_eq!(out.status.code(), Some(0), "tier {tier}: {}", stderr(out));
}

#[test]
fn busybox_lists_the_root_directory() {
    for (tier, out) in run(&["/bin/busybox", "ls", "/"]) {
        assert_ok(tier, &out);
        let names: Vec<&str> = stdout(&out).lines().collect();
        for dir in ["bin", "etc", "lib", "root", "tmp", "usr", "var"] {
            assert!(names.contains(&dir), "tier {tier}: {names:?}");
        }
    }
}

#[test]
fn applets_run_through_symlinks() {
    for (tier, out) in run(&["/bin/ls", "-l", "/bin/sh", "/etc"]) {
        assert_ok(tier, &out);
        assert!(
            stdout(&out).contains("/bin/sh -> /bin/busybox"),
            "tier {tier}: {}",
            stdout(&out)
        );
        assert!(
            stdout(&out).contains("os-release"),
            "tier {tier}: {}",
            stdout(&out)
        );
    }
}

#[test]
fn cat_reads_files_and_follows_symlinks() {
    // /etc/os-release 是指向 ../usr/lib/os-release 的相对符号链接
    for (tier, out) in run(&["/bin/cat", "/etc/os-release"]) {
        assert_ok(tier, &out);
        assert!(
            stdout(&out).contains("ID=alpine\n"),
            "tier {tier}: {}",
            stdout(&out)
        );
    }
}

#[test]
fn uname_reports_the_abi_contract() {
    for (tier, out) in run(&["/bin/uname", "-srm"]) {
        assert_ok(tier, &out);
        assert_eq!(stdout(&out), "Linux 6.1.0-appine aarch64\n", "tier {tier}");
    }
}

#[test]
fn checksums_agree_between_tiers() {
    let results = run(&[
        "/usr/bin/sha256sum",
        "/bin/busybox",
        "/lib/ld-musl-aarch64.so.1",
    ]);
    for (tier, out) in &results {
        assert_ok(tier, out);
        assert_eq!(
            stdout(out).lines().count(),
            2,
            "tier {tier}: {}",
            stdout(out)
        );
    }
    if let [(_, t0), (_, t1)] = &results[..] {
        assert_eq!(stdout(t0), stdout(t1));
    }
}

#[test]
fn shell_builtins_redirect_into_the_store() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "cd /tmp && f=redirect && echo hi > $f && echo more >> $f && \
         while read l; do echo line $l; done < $f && pwd",
    ]) {
        assert_ok(tier, &out);
        assert_eq!(stdout(&out), "line hi\nline more\n/tmp\n", "tier {tier}");
    }
}

#[test]
fn files_and_directories_can_be_created() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let dir = format!("/tmp/mkdir-{tier}");
        let out = command(store, tier, &["/bin/mkdir", "-p", &format!("{dir}/a/b")])
            .output()
            .unwrap();
        assert_ok(tier, &out);
        let out = command(
            store,
            tier,
            &["/bin/cp", "/etc/hostname", &format!("{dir}/a/b/copy")],
        )
        .output()
        .unwrap();
        assert_ok(tier, &out);
        let out = command(store, tier, &["/usr/bin/find", &dir])
            .output()
            .unwrap();
        assert_ok(tier, &out);
        let expected = format!("{dir}\n{dir}/a\n{dir}/a/b\n{dir}/a/b/copy\n");
        assert_eq!(stdout(&out), expected, "tier {tier}");
    }
}

#[test]
fn stdin_reaches_the_guest() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let mut child = command(store, tier, &["/bin/sh", "-c", "read v; echo got $v"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"typed\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert_ok(tier, &out);
        assert_eq!(stdout(&out), "got typed\n", "tier {tier}");
    }
}

#[test]
fn idle_readers_of_host_stdin_sleep_through_other_traffic() {
    // 不带 --pty 时 guest 的 stdin 是宿主的管道：阻塞在 read 里的进程只等宿主 fd，shell
    // 在一个 FIFO 上的读写不叫醒它（自愿上下文切换的次数不变）；写进 stdin 之后照常读到。
    // 用 dd 读：busybox 的 read 内建命令先 poll，测的就不是阻塞的读了。读写 FIFO 只用
    // 内建命令：musl 的 fork 在 clone 前后屏蔽所有信号，这时有子进程退出的话 SIGCHLD 被所有
    // 线程屏蔽，仍会叫醒所有等待者
    let Some(store) = store() else {
        return;
    };
    // 先等 dd 停在读里：已经 exec 成 dd、在内核里等着（S）、切换次数 0.2 秒没变（全量测试
    // 并行时它可能还在启动）。读状态只用 shell 的内建命令，不起进程、不用管道
    let script = "mkfifo /tmp/hoststdin.fifo
        exec 3<&0 4<>/tmp/hoststdin.fifo
        rm /tmp/hoststdin.fifo
        dd bs=5 count=1 <&3 >/tmp/hoststdin.out 2>/dev/null &
        r=$!
        n() {
            while read k v w; do
                case $k in Name:) m=$v ;; State:) s=$v ;; voluntary_ctxt_switches:) c=$v ;; esac
            done </proc/$r/status
        }
        n; b=$c; t=0
        while sleep 0.2; n; [ \"$m $s $c\" != \"dd S $b\" ] && [ $t -lt 50 ]; do b=$c; t=$((t + 1)); done
        i=0
        while [ $i -lt 200 ]; do echo x >&4; read l <&4; i=$((i + 1)); done
        n
        echo \"woken $((c - b))\"
        wait $r
        echo \"reader got $(cat /tmp/hoststdin.out)\"
        rm /tmp/hoststdin.out";
    for tier in TIERS {
        let mut child = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let woken = lines.next().unwrap().unwrap();
        child.stdin.take().unwrap().write_all(b"typed\n").unwrap();
        let rest: Vec<String> = lines.map(Result::unwrap).collect();
        assert_eq!(child.wait().unwrap().code(), Some(0), "tier {tier}");
        assert_eq!(woken, "woken 0", "tier {tier}");
        assert_eq!(rest, ["reader got typed"], "tier {tier}");
    }
}

#[test]
fn environment_reaches_the_guest() {
    for (tier, out) in run(&["/usr/bin/env"]) {
        assert_ok(tier, &out);
        assert_eq!(stdout(&out), format!("{PATH}\n"), "tier {tier}");
    }
}

#[test]
fn exit_status_and_errors_are_propagated() {
    for (tier, out) in run(&["/bin/sh", "-c", "exit 7"]) {
        assert_eq!(out.status.code(), Some(7), "tier {tier}: {}", stderr(&out));
    }
    for (tier, out) in run(&["/bin/ls", "/nonexistent"]) {
        assert_eq!(out.status.code(), Some(1), "tier {tier}");
        assert!(
            stderr(&out).contains("No such file or directory"),
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn fatal_signals_write_core_files() {
    // RLIMIT_CORE 打开时，SIGSEGV 结束的进程在当前目录留下 ELF core 文件（ET_CORE、
    // aarch64）；限制是 0 时不写；core_pattern 里的 %e、%s 展开成程序名和信号
    let script = "mkdir -p /tmp/cores && cd /tmp/cores && rm -f core* && ulimit -c unlimited && \
                  sh -c 'kill -SEGV $$'; echo status $?; od -A n -t x1 -N 4 core; \
                  od -A n -t d2 -j 16 -N 4 core; rm core; ulimit -c 0; \
                  sh -c 'kill -SEGV $$'; echo status $?; test -e core && echo present || echo absent; \
                  ulimit -c unlimited; echo 'core.%e.%s' > /proc/sys/kernel/core_pattern; \
                  sh -c 'kill -ABRT $$'; ls core.*; cat /proc/self/coredump_filter";
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        let words: Vec<String> = shell(tier, &out)
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            words,
            [
                "status",
                "139",
                "7f",
                "45",
                "4c",
                "46",
                "4",
                "183",
                "status",
                "139",
                "absent",
                "core.sh.6",
                "00000033"
            ],
            "tier {tier}"
        );
    }
}

/// 用 musl-gcc 静态编译 tests/guest/<name>.c；没有 musl-gcc 时返回 None。
fn build_static(name: &str) -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/guest")
        .join(format!("{name}.c"));
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rootfs-{name}"));
    let status = Command::new("musl-gcc")
        .args([
            "-static", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread", "-o",
        ])
        .arg(&out)
        .arg(&src)
        .status();
    let Ok(status) = status else {
        assert!(
            std::env::var_os("APPINE_REQUIRE_MUSL").is_none_or(|v| v != "1"),
            "musl-gcc not found; install musl-tools"
        );
        eprintln!("skipping {name}: musl-gcc not found");
        return None;
    };
    assert!(status.success(), "compiling {} failed", src.display());
    Some(out)
}

#[test]
fn core_files_have_the_linux_layout() {
    // 多线程的进程崩溃：core 文件的程序头、MADV_DONTDUMP、每个线程的 note、core_pattern、
    // RLIMIT_CORE 的截断（tests/guest/coredump.c），程序要拷进仓库里运行
    let (Some(store), Some(program)) = (store(), build_static("coredump")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/coredump && chmod +x /tmp/coredump && /tmp/coredump";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
    }
}

#[test]
fn each_step_of_finishing_a_process_survives_kernel_bugs() {
    // 进程结束的一步撞上内核层的 bug（debug 构建的测试钩子模拟）时，只丢掉这一步做的事，
    // 后面的步骤照样做（tests/guest/finish.c）；要有控制终端，程序拷进仓库里运行
    if !cfg!(debug_assertions) {
        return;
    }
    let (Some(store), Some(program)) = (store(), build_static("finish")) else {
        return;
    };
    for tier in TIERS {
        let script = "cat > /tmp/finish && chmod +x /tmp/finish && /tmp/finish";
        let out = command(store, tier, &["/bin/sh", "-c", script])
            .env("APPINE_TEST_HOOKS", "1")
            .env("APPINE_ON_BUG", "kill")
            .stdin(std::fs::File::open(&program).unwrap())
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success() && text.ends_with("all ok\n"),
            "tier {tier}: {text}{}",
            stderr(&out)
        );
        for step in [
            "releasing AIO contexts",
            "handing its children to init",
            "sending parent-death signals",
            "releasing SysV IPC",
            "releasing the controlling terminal",
            "reporting to its tracer",
            "handing it to its parent",
            "releasing its tracees",
            "signalling orphaned process groups",
        ] {
            let report = format!("was exiting ({step})");
            assert!(
                stderr(&out).contains(&report),
                "tier {tier}: {}",
                stderr(&out)
            );
        }
    }
}

#[test]
fn missing_or_non_executable_programs_fail_to_start() {
    for (tier, out) in run(&["/bin/no-such-program"]) {
        assert_eq!(out.status.code(), Some(127), "tier {tier}");
    }
    for (tier, out) in run(&["/etc/passwd"]) {
        assert_eq!(out.status.code(), Some(126), "tier {tier}");
    }
}

/// 在 `sh -c` 里运行脚本，要求退出码为 0，返回 stdout。
fn shell(tier: &str, out: &Output) -> String {
    assert_ok(tier, out);
    stdout(out).to_owned()
}

#[test]
fn file_permissions_apply_to_other_users() {
    // su 到 nobody 之后：读不了 root 的 600 文件，进不了 /root，删不了粘滞位 /tmp 里
    // 别人的文件；新文件属于 nobody
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "f=/tmp/dac-$$; umask 077; echo secret > $f; \
         su -s /bin/sh nobody -c \"id -u; cat $f || echo denied; ls /root || echo denied; \
         rm -f $f || echo denied; touch $f.n; stat -c %u:%a $f.n; rm $f.n\"; \
         chmod 644 $f; su -s /bin/sh nobody -c \"cat $f\"; rm $f",
    ]) {
        assert_eq!(
            shell(tier, &out),
            "65534\ndenied\ndenied\ndenied\n65534:600\nsecret\n",
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn pipelines_connect_processes() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "ls / | wc -l; echo hello | tr a-z A-Z | cat; cat /etc/os-release | grep ^ID= | sort | uniq",
    ]) {
        let out = shell(tier, &out);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].trim().parse::<u32>().unwrap() >= 10,
            "tier {tier}: {out}"
        );
        assert_eq!(&lines[1..], ["HELLO", "ID=alpine"], "tier {tier}");
    }
}

#[test]
fn subshells_and_command_substitution_fork() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "echo $(echo nested $(echo twice)); (exit 3); echo status $?; false; echo $?",
    ]) {
        assert_eq!(
            shell(tier, &out),
            "nested twice\nstatus 3\n1\n",
            "tier {tier}"
        );
    }
}

#[test]
fn trap_handlers_run_on_signals() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "trap 'echo caught USR1' USR1; kill -USR1 $$; echo after; \
         trap 'echo term; exit 5' TERM; trap 'echo bye' EXIT; kill -TERM $$; echo not reached",
    ]) {
        // 与原生的 Alpine busybox 相同
        assert_eq!(
            stdout(&out),
            "caught USR1\nafter\nterm\nbye\n",
            "tier {tier}"
        );
        assert_eq!(out.status.code(), Some(5), "tier {tier}");
    }
    // 没有 trap 的 SIGTERM 按默认动作终止 shell，EXIT trap 不会运行
    for (tier, out) in run(&["/bin/sh", "-c", "trap 'echo bye' EXIT; kill $$; echo no"]) {
        assert_eq!(stdout(&out), "", "tier {tier}");
        assert_eq!(out.status.code(), Some(128 + 15), "tier {tier}");
    }
}

#[test]
fn find_exec_and_xargs_spawn_programs() {
    // busybox 用 vfork 启动 -exec 和 xargs 的命令
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "find /etc -name hostname -exec cat {} \\; ; echo /etc/hostname | xargs cat",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(out.lines().count(), 2, "tier {tier}: {out}");
        let first = out.lines().next().unwrap();
        assert!(out.lines().all(|l| l == first), "tier {tier}: {out}");
    }
}

#[test]
fn background_jobs_can_be_waited_for_and_killed() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "sleep 0.1 & echo started; wait; echo done; \
         sleep 10 & kill $!; wait $!; echo killed $?",
    ]) {
        assert_eq!(
            shell(tier, &out),
            "started\ndone\nkilled 143\n",
            "tier {tier}"
        );
    }
}

#[test]
fn closed_pipes_stop_writers_with_sigpipe() {
    // yes 永远在写：head 退出之后它只能被 SIGPIPE 终止
    for (tier, out) in run(&["/bin/sh", "-c", "yes | head -n 2; echo end"]) {
        assert_eq!(shell(tier, &out), "y\ny\nend\n", "tier {tier}");
    }
}

#[test]
fn scripts_run_through_their_interpreter() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "cd /tmp && printf '#!/bin/sh -e\\necho script $0 $1\\n' > s && chmod +x s && \
         ./s arg && exec ./s exec",
    ]) {
        assert_eq!(
            shell(tier, &out),
            "script ./s arg\nscript ./s exec\n",
            "tier {tier}"
        );
    }
}

#[test]
fn files_can_be_renamed_linked_and_removed() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let script = format!(
            "cd /tmp && mkdir ns{tier} && cd ns{tier} && echo data > a && mv a b && \
             ln -s b sym && ln b hard && cat sym && stat -c '%h %a' b && chmod 600 b && \
             stat -c '%a' hard && rm b sym && cat hard && rm hard && cd .. && rmdir ns{tier} && \
             ls -d ns{tier} 2>&1"
        );
        let out = command(store, tier, &["/bin/sh", "-c", &script])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "tier {tier}: {}", stderr(&out));
        assert_eq!(
            stdout(&out),
            format!("data\n2 644\n600\ndata\nls: ns{tier}: No such file or directory\n"),
            "tier {tier}"
        );
    }
}

#[test]
fn device_nodes_work() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "echo hidden > /dev/null; cat /dev/null; head -c 5 /dev/zero | od -An -tx1; \
         head -c 16 /dev/urandom | wc -c; echo x > /dev/full || echo full",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.split_whitespace().collect::<Vec<_>>(),
            ["00", "00", "00", "00", "00", "16", "full"],
            "tier {tier}: {out}"
        );
    }
}

#[test]
fn procfs_describes_processes() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        // 后台的子进程可能还没 exec 成 sleep（comm 还是 sh），与 Linux 相同：先等它
        "sleep 5 & i=0; until ps -o comm | grep -qx sleep || [ $i -ge 200 ]; do i=$((i+1)); done; \
         ps -o pid,comm | grep -c sleep; grep ^Name /proc/self/status; \
         readlink /proc/self/exe; cat /proc/self/cmdline | tr '\\0' ' '; echo; \
         df / | tail -n 1 | grep -c ' /$'; grep -c ^processor /proc/cpuinfo | grep -vc ^0; \
         echo piped | cat /dev/stdin; kill $!",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "1",
                "Name:\tgrep",
                // 与 Linux 相同，exe 是解析完符号链接的路径
                "/bin/busybox",
                "cat /proc/self/cmdline ",
                "1",
                "1",
                "piped"
            ],
            "tier {tier}"
        );
    }
}

#[test]
fn sysfs_and_limits_describe_the_machine() {
    // busybox 的 nproc --all 数 /sys/devices/system/cpu 下的 cpu<n>；/proc/self/limits 与
    // ulimit 一致
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "n=$(grep -c ^processor /proc/cpuinfo); test $(nproc --all) = $n && echo nproc; \
         online=$(cat /sys/devices/system/cpu/online); \
         test $online = 0-$((n - 1)) -o $online = 0 -a $n = 1 && echo online; \
         ls /sys/devices/system/cpu/cpu0/topology | grep -c _list; \
         ulimit -n 777; grep 'open files' /proc/self/limits | tr -s ' '; \
         grep -c ' - sysfs ' /proc/self/mountinfo",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            ["nproc", "online", "5", "Max open files 777 777 files ", "1"],
            "tier {tier}"
        );
    }
}

#[test]
fn network_configuration_is_visible() {
    // netlink（ip）、接口 ioctl（ifconfig）、/proc/net/dev、指向 DNS 存根的 resolv.conf
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "ip addr show lo | grep -o 'inet 127.0.0.1/8'; ifconfig lo | grep -o 'addr:127.0.0.1'; \
         grep -c ' lo:' /proc/net/dev; cat /etc/resolv.conf",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "inet 127.0.0.1/8",
                "addr:127.0.0.1",
                "1",
                "nameserver 127.0.0.53"
            ],
            "tier {tier}"
        );
    }
}

#[test]
fn syslog_messages_travel_over_a_unix_socket() {
    // syslogd 把数据报 socket 绑定在 /dev/log 上，logger 按路径发过去
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "rm -f /tmp/syslog.txt; syslogd -n -O /tmp/syslog.txt & sleep 0.5; \
         test -S /dev/log && echo socket; logger -t appine hello; sleep 0.5; kill $!; wait; \
         grep -o 'appine: hello' /tmp/syslog.txt",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            ["socket", "appine: hello"],
            "tier {tier}"
        );
    }
}

#[test]
fn flock_excludes_other_holders() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "flock /tmp/flock-test -c 'sleep 1' & sleep 0.3; flock -n /tmp/flock-test -c true; \
         echo busy=$?; wait; flock -n /tmp/flock-test -c true; echo free=$?",
    ]) {
        let out = shell(tier, &out);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            ["busy=1", "free=0"],
            "tier {tier}"
        );
    }
}

#[test]
fn stop_and_continue_signals() {
    for (tier, out) in run(&[
        "/bin/sh",
        "-c",
        "sleep 5 & p=$!; kill -STOP $p; sleep 0.2; grep State /proc/$p/status; \
         kill -CONT $p; sleep 0.2; grep State /proc/$p/status; kill $p; wait $p; echo $?",
    ]) {
        assert_eq!(
            stdout(&out),
            "State:\tT (stopped)\nState:\tS (sleeping)\n143\n",
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

/// 在进程内 PTY 上运行的交互式 shell。
struct Interactive {
    child: Child,
    stdin: ChildStdin,
    output: Arc<Mutex<Vec<u8>>>,
    /// 已经匹配过的输出长度：`expect` 只在它之后查找。
    seen: usize,
    tier: &'static str,
}

const PROMPT: &str = "appine$ ";

impl Interactive {
    fn start(store: &Path, tier: &'static str) -> Interactive {
        let mut child = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args([
                "--tier",
                tier,
                "--pty",
                "--env",
                PATH,
                "--env",
                "TERM=xterm",
            ])
            .arg(format!("--env=PS1={PROMPT}"))
            .arg("--rootfs")
            .arg(store)
            .args(["--", "/bin/sh", "-i"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let mut s = Interactive {
            child,
            stdin,
            output,
            seen: 0,
            tier,
        };
        s.expect(PROMPT);
        s
    }

    fn send(&mut self, keys: &[u8]) {
        self.stdin.write_all(keys).unwrap();
        self.stdin.flush().unwrap();
    }

    /// 等到新的输出里出现 `text`。
    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            {
                let out = self.output.lock().unwrap();
                let rest = &out[self.seen..];
                if let Some(i) = rest.windows(text.len()).position(|w| w == text.as_bytes()) {
                    self.seen += i + text.len();
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "tier {}: timed out waiting for {text:?}; output so far:\n{}",
                    self.tier,
                    String::from_utf8_lossy(&out)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 退出 shell，返回它的退出码。shell 不肯退出时（比如还有停住的作业）不一直等下去。
    fn exit(mut self) -> Option<i32> {
        self.send(b"exit\n");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                Instant::now() < deadline,
                "tier {}: the shell did not exit; output so far:\n{}",
                self.tier,
                String::from_utf8_lossy(&self.output.lock().unwrap())
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Interactive {
    /// 用例失败时不留下还在等输入的 appine-run。
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn interactive_shell_runs_on_a_terminal() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let mut s = Interactive::start(store, tier);
        s.send(b"echo $((6*7)) | cat; tty; stty size\n");
        s.expect("42\r\n");
        s.expect("/dev/pts/0\r\n");
        s.expect("24 80\r\n");
        s.expect(PROMPT);
        assert_eq!(s.exit(), Some(0), "tier {tier}");
    }
}

#[test]
fn ctrl_c_and_ctrl_z_control_the_foreground_job() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let mut s = Interactive::start(store, tier);
        // 子进程打印 job-42 时已经是前台进程组；输出的文字不能出现在命令行里，
        // 否则会先匹配到回显。作业停在读终端上
        s.send(b"sh -c 'echo job-$((6*7)); while read x; do echo got-$x; done'\n");
        s.expect("job-42");
        s.send(b"\x1a");
        s.expect("Stopped");
        s.expect(PROMPT);
        s.send(b"jobs\n");
        s.expect("Stopped");
        s.expect(PROMPT);
        // ash 的 fg 先打印作业的命令，再把终端交给它、让它继续。看到命令就按 ^C 的话，
        // SIGINT 可能落到 shell 自己身上：shell 回到提示符，$? 也是 130，作业却一直停着，
        // exit 不肯退出（原生 Linux 上也一样）。所以等作业读到一行、确实在前台运行了再按
        // fg 打印的命令换成了双引号，`$` 前面加了反斜杠
        s.send(b"fg\n");
        s.expect("; done");
        s.send(b"ping\n");
        s.expect("got-ping");
        s.send(b"\x03");
        s.expect(PROMPT);
        s.send(b"echo status $?\n");
        s.expect("status 130");
        s.expect(PROMPT);
        assert_eq!(s.exit(), Some(0), "tier {tier}");
    }
}

#[test]
fn vi_edits_a_file() {
    let Some(store) = store() else {
        return;
    };
    for tier in TIERS {
        let mut s = Interactive::start(store, tier);
        let file = format!("/tmp/vi-{tier}");
        s.send(format!("rm -f {file}; vi {file}\n").as_bytes());
        // 切到备用屏幕，画出状态行
        s.expect("\x1b[?1049h");
        s.expect("100%");
        // vi 靠 ESC 之后有没有紧跟着的字节区分 ESC 键和转义序列，所以分开发送
        s.send(b"ihello from vi");
        std::thread::sleep(Duration::from_millis(300));
        s.send(b"\x1b");
        std::thread::sleep(Duration::from_millis(300));
        s.send(b"oline two");
        std::thread::sleep(Duration::from_millis(300));
        s.send(b"\x1b");
        std::thread::sleep(Duration::from_millis(300));
        s.send(b":wq\r");
        s.expect("\x1b[?1049l");
        s.expect(PROMPT);
        s.send(format!("cat {file}\n").as_bytes());
        s.expect("hello from vi\r\nline two\r\n");
        s.expect(PROMPT);
        assert_eq!(s.exit(), Some(0), "tier {tier}");
    }
}

#[test]
fn apk_installs_packages_and_git_clones_over_https() {
    if std::env::var_os("APPINE_NETWORK_TESTS").is_none_or(|v| v != "1") {
        eprintln!("skipping network test: set APPINE_NETWORK_TESTS=1 to enable it");
        return;
    }
    let Some(tarball) = find_tarball() else {
        panic!("rootfs tarball not found; run scripts/fetch-rootfs");
    };
    // 安装软件包会改动很多文件：用自己的仓库，不影响其他用例
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("network-store");
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
        .arg("--rootfs")
        .arg(&dir)
        .arg("--import")
        .arg(&tarball)
        .output()
        .unwrap();
    assert!(out.status.success(), "import failed: {}", stderr(&out));

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_appine-run"));
    // Tier 0 太慢，只在 Tier 1 下运行
    cmd.args(["--tier", "1", "--env", PATH, "--env", "HOME=/root"]);
    for var in ["http_proxy", "https_proxy", "no_proxy"] {
        if let Ok(v) = std::env::var(var) {
            cmd.arg("--env").arg(format!("{var}={v}"));
        }
    }
    let out = cmd
        .arg("--rootfs")
        .arg(&dir)
        .args([
            "--",
            "/bin/sh",
            "-c",
            "apk update && apk add git python3 && \
             git clone --depth 1 https://github.com/octocat/Hello-World.git /tmp/hello && \
             cat /tmp/hello/README && \
             python3 -c 'import asyncio, hashlib, sqlite3, ssl; print(\"python ok\")'",
        ])
        .output()
        .unwrap();
    let text = shell("1", &out);
    assert!(text.contains("Hello World!"), "{text}");
    assert!(text.ends_with("python ok\n"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn importing_into_an_existing_store_fails() {
    let (Some(store), Some(tarball)) = (store(), find_tarball()) else {
        return;
    };
    let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
        .arg("--rootfs")
        .arg(store)
        .arg("--import")
        .arg(tarball)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
}

/// 读控制台直到会话结束（Android 的终端标签页就是这样用 Machine 的）。
fn read_console(machine: &appine_kernel::Machine, id: u32) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match machine.read_console(id, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8(out).unwrap().replace("\r\n", "\n")
}

#[test]
fn threads_max_limits_root_too() {
    // threads-max 对 root 也有效：线程数到了上限时 fork 返回 EAGAIN，busybox sh 报告
    // "can't fork" 后退出；写入的值不能小于 20
    let script = "echo 19 > /proc/sys/kernel/threads-max || echo rejected; \
                  echo 25 > /proc/sys/kernel/threads-max; cat /proc/sys/kernel/threads-max; \
                  i=0; while [ $i -lt 40 ]; do sleep 5 & i=$((i + 1)); done; echo not reached";
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        assert_eq!(
            stdout(&out),
            "rejected\n25\n",
            "tier {tier}: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("can't fork"),
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn rlimit_stack_at_execve_sets_the_stack_and_layout() {
    // 初始栈只映射参数下面再多 128 KB；参数最多占栈大小上限的 1/4（至少 128 KB）；不限
    // 栈大小时 mmap 区让出 MAX_GAP（ld-musl 映射在 0x1555_5555_5000 以下）
    let script = "grep stack /proc/self/maps; \
                  (ulimit -s 1024; /bin/busybox true $(seq 20000) 2>/dev/null || echo E2BIG; \
                   /bin/busybox true $(seq 10000) && echo small ok); \
                  /bin/busybox true $(seq 20000) && echo default ok; \
                  (ulimit -s unlimited; grep -m1 ld-musl /proc/self/maps); \
                  grep -m1 ld-musl /proc/self/maps";
    let start_of = |line: &str| u64::from_str_radix(line.split('-').next().unwrap(), 16).unwrap();
    for (tier, out) in run(&["/bin/sh", "-c", script]) {
        let text = shell(tier, &out);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6, "tier {tier}: {text}");
        let (lo, hi) = lines[0].split_once(' ').unwrap().0.split_once('-').unwrap();
        let (lo, hi) = (
            u64::from_str_radix(lo, 16).unwrap(),
            u64::from_str_radix(hi, 16).unwrap(),
        );
        assert!(lines[0].ends_with("[stack]"), "tier {tier}: {text}");
        assert!(hi == 1 << 47 && hi - lo <= 256 << 10, "tier {tier}: {text}");
        assert_eq!(
            &lines[1..4],
            ["E2BIG", "small ok", "default ok"],
            "tier {tier}"
        );
        assert!(start_of(lines[4]) < 0x1555_5555_5000, "tier {tier}: {text}");
        assert!(start_of(lines[5]) > 0x7f00_0000_0000, "tier {tier}: {text}");
    }
}

#[test]
fn lowering_oom_score_adj_needs_cap_sys_resource() {
    // 普通用户能调高 oom_score_adj、再调回下限，不能调到下限以下；root 写的值成为新的
    // 下限，su 之后继承
    let f = "/proc/self/oom_score_adj";
    let script = format!(
        "su -s /bin/sh nobody -c 'echo 500 > {f}; cat {f}; echo 0 > {f}; \
         echo -1 > {f} || echo denied; cat {f}'; \
         echo -500 > {f}; su -s /bin/sh nobody -c 'cat {f}; echo -400 > {f}; \
         echo -500 > {f} && echo ok; echo -501 > {f} || echo denied'"
    );
    for (tier, out) in run(&["/bin/sh", "-c", &script]) {
        assert_eq!(
            shell(tier, &out),
            "500\ndenied\n0\n-500\nok\ndenied\n",
            "tier {tier}: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("Permission denied"),
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn oom_killer_honours_oom_score_adj() {
    // 超出内存上限时先结束 oom_score_adj 为 1000 的进程（即使它几乎不占内存），等它退出
    // 之后还超出，再结束真正吃内存的 awk。挑进程的规则与执行层无关，只在 Tier 1 下跑：
    // Tier 0 的 debug 构建拼 128 MB 的字符串要十几秒
    let Some(store) = store() else {
        return;
    };
    let script = "(echo 1000 > /proc/self/oom_score_adj && exec sleep 30) & sleep 0.2; \
                  awk 'BEGIN { s = \"x\"; for (i = 0; i < 27; i++) s = s s; print length(s) }'; \
                  echo awk $?; wait $!; echo sleep $?";
    let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
        .args([
            "--tier",
            "1",
            "--memory-limit",
            "128M",
            "--env",
            PATH,
            "--rootfs",
        ])
        .arg(store)
        .args(["--", "/bin/sh", "-c", script])
        .output()
        .unwrap();
    assert_eq!(stdout(&out), "awk 137\nsleep 137\n", "{}", stderr(&out));
    let err = stderr(&out);
    let (sleep, awk) = (err.find("(sleep) rss"), err.find("(awk) rss"));
    assert!(sleep.is_some() && sleep < awk, "{err}");
}

#[test]
fn cgroup_memory_files_follow_the_oom_killer() {
    // /sys/fs/cgroup/memory.max 是 --memory-limit 给的上限（none 时是 max）；OOM killer 结束了
    // 进程之后 memory.events 记着，memory.peak 量到过超出上限的用量。只在 Tier 1 下跑，理由
    // 同上
    let Some(store) = store() else {
        return;
    };
    let run = |limit: &str, script: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args([
                "--tier",
                "1",
                "--memory-limit",
                limit,
                "--env",
                PATH,
                "--rootfs",
            ])
            .arg(store)
            .args(["--", "/bin/sh", "-c", script])
            .output()
            .unwrap();
        stdout(&out).to_owned()
    };
    let none = run("none", "cat /sys/fs/cgroup/memory.max");
    assert_eq!(none, "max\n");
    let script = "cd /sys/fs/cgroup; cat memory.max; grep oom_kill memory.events; \
                  awk 'BEGIN { s = \"x\"; for (i = 0; i < 27; i++) s = s s; print length(s) }'; \
                  echo awk $?; grep -E '^(max|oom|oom_kill) ' memory.events | sed 's/ [1-9][0-9]*$/ N/'; \
                  [ $(cat memory.peak) -gt 134217728 ] && echo peak over the limit";
    assert_eq!(
        run("128M", script),
        "134217728\noom_kill 0\nawk 137\nmax N\noom N\noom_kill N\npeak over the limit\n"
    );
}

#[test]
fn one_machine_runs_several_terminal_sessions() {
    use appine_kernel::{ExitStatus, Machine, MachineOptions, Stdio, Tier};
    let Some(store) = store() else {
        return;
    };
    for tier in [Tier::Interpreter, Tier::Dbt] {
        let machine = Machine::start(&MachineOptions {
            tier,
            rootfs: Some(store.to_path_buf()),
            memory_limit: None,
        })
        .unwrap();
        let env = [PATH.as_bytes().to_vec()];
        let sh = |script: &str| {
            let id = machine.open_console(24, 80).unwrap();
            let argv = [b"sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()];
            let pid = machine
                .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
                .unwrap();
            (id, pid)
        };
        // 两个会话同时运行，各自有自己的控制终端
        let (a, pa) = sh("tty; stty size; echo one");
        let (b, pb) = sh("read line; echo got $line; exit 3");
        machine.write_console(b, b"input\n").unwrap();
        machine.resize_console(a, 40, 100).unwrap();
        let (out_a, out_b) = (read_console(&machine, a), read_console(&machine, b));
        assert!(out_a.starts_with("/dev/pts/"), "{tier:?}: {out_a}");
        assert!(out_a.ends_with("one\n"), "{tier:?}: {out_a}");
        assert_eq!(out_b, "input\ngot input\n", "{tier:?}");
        assert_eq!(machine.wait(pa), ExitStatus::Exited(0));
        assert_eq!(machine.wait(pb), ExitStatus::Exited(3));
        // 关闭控制台就是挂断终端：交互式 shell 收到 SIGHUP 退出
        let id = machine.open_console(24, 80).unwrap();
        let argv = [b"sh".to_vec(), b"-i".to_vec()];
        let pid = machine
            .spawn(b"/bin/sh", &argv, &env, Stdio::Console(id))
            .unwrap();
        machine.write_console(id, b"echo ready\n").unwrap();
        machine.close_console(id).unwrap();
        assert!(
            matches!(machine.wait(pid), ExitStatus::Signaled { signal, .. } if signal.number() == 1),
            "{tier:?}"
        );
        assert_eq!(
            machine.read_console(id, &mut [0; 8]),
            Err(appine_kernel::errno::Errno::ENOENT)
        );
    }
}
