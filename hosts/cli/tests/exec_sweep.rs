//! Integration tests verifying that post-syscall sweeps run on successful `execve` / `execveat`
//! without requiring a subsequent BKL syscall in the new program.
//!
//! Producing Linux aarch64 ELFs needs a Linux arm64 host; on other hosts the file is not compiled.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TIERS: [&str; 2] = ["0", "1"];

#[test]
fn exec_runs_post_syscall_sweeps() {
    let Some(program) = build_c_guest("exec_sweep") else {
        return;
    };
    let bytes = std::fs::read(&program).unwrap();
    let (_, store) = output("exec_sweep-store");
    let (_, empty_dir) = output("exec_sweep-empty");
    let (_, tar_path) = output("exec_sweep-empty.tar.gz");
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&empty_dir);
    std::fs::create_dir_all(&empty_dir).unwrap();
    let tar = Command::new("tar")
        .args(["-czf"])
        .arg(&tar_path)
        .args(["-C"])
        .arg(&empty_dir)
        .arg(".")
        .status()
        .expect("tar failed");
    assert!(tar.success());
    appine_kernel::import_rootfs(&store, &tar_path).unwrap();
    let guest_path = "/exec_sweep";
    appine_kernel::write_file(&store, guest_path, &bytes, 0o755, true, 0, 0).unwrap();

    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--rootfs"])
            .arg(&store)
            .args(["--tier", tier, guest_path])
            .output()
            .expect("failed to run appine-run");
        assert_eq!(
            out.status.code(),
            Some(0),
            "tier {tier}: stdout={} stderr={}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            stdout(&out).ends_with("all ok\n"),
            "tier {tier}: stdout={} stderr={}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            !stderr(&out).contains("appine: internal error"),
            "tier {tier}: unexpected internal error in stderr: {}",
            stderr(&out)
        );
    }

    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&empty_dir);
    let _ = std::fs::remove_file(&tar_path);
}

fn build_c_guest(name: &str) -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/guest")
        .join(format!("{name}.c"));
    let (out, tmp) = output(name);
    let status = Command::new("musl-gcc")
        .args([
            "-static", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread", "-o",
        ])
        .arg(&tmp)
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
    std::fs::rename(&tmp, &out).unwrap();
    Some(out)
}

fn output(name: &str) -> (PathBuf, PathBuf) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let tmp = dir.join(format!("guest-{name}.{}.{n}", std::process::id()));
    (dir.join(format!("guest-{name}")), tmp)
}

fn stdout(out: &Output) -> &str {
    std::str::from_utf8(&out.stdout).unwrap()
}

fn stderr(out: &Output) -> &str {
    std::str::from_utf8(&out.stderr).unwrap()
}
