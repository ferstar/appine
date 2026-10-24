//! 端到端测试：把 Alpine minirootfs 导入 inode 仓库，经由 `ld-musl` 运行 busybox
//! （M1b、M3a、M3b 的退出标准）。每个用例都分别在 Tier 0 和 Tier 1 下运行。
//!
//! 交互式的用例用 `appine-run --pty` 在进程内 PTY 上运行 `sh -i`，经由管道发送按键
//! （^C、^Z 就是字节 0x03、0x1a），等到输出里出现预期的文字再发下一步。
//!
//! rootfs 由 `scripts/fetch-rootfs` 下载到 `target/rootfs/`，也可以用环境变量
//! `APPINE_ROOTFS_TARBALL` 指定。找不到时跳过这些测试；设置
//! `APPINE_REQUIRE_ROOTFS=1`（CI 这样做）时改为失败。
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::io::{Read, Write};
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
        "sleep 5 & ps -o pid,comm | grep -c sleep; grep ^Name /proc/self/status; \
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
                "/usr/bin/readlink",
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

    /// 退出 shell，返回它的退出码。
    fn exit(mut self) -> Option<i32> {
        self.send(b"exit\n");
        self.child.wait().unwrap().code()
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
        // 否则会先匹配到回显
        s.send(b"sh -c 'echo job-$((6*7)); exec sleep 30'\n");
        s.expect("job-42");
        s.send(b"\x1a");
        s.expect("Stopped");
        s.expect(PROMPT);
        s.send(b"jobs\n");
        s.expect("Stopped");
        s.expect(PROMPT);
        // fg 打印作业的命令之前已经把终端交给了它
        s.send(b"fg\n");
        s.expect("exec sleep 30");
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
