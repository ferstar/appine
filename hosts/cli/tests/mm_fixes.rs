//! Standalone integration tests for kernel/mm Batch-1 fixes (shared_maps visibility before PTEs
//! and Linux 6.1 gap semantics for madvise, msync, mlock/munlock, and mincore), run under both
//! Tier 0 and Tier 1 inside a minimal rootfs.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::process::Command;

const TIERS: [&str; 2] = ["0", "1"];

#[test]
fn mm_gap_semantics_and_shared_futex_maps() {
    let Some(program) = build_c_guest("mm_fixes") else {
        return;
    };
    let bytes = std::fs::read(&program).unwrap();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let pid = std::process::id();
    let store = dir.join(format!("mm-fixes-store.{pid}"));
    let empty_dir = dir.join(format!("mm-fixes-empty.{pid}"));
    let tar_path = dir.join(format!("mm-fixes-empty.{pid}.tar.gz"));
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
    let guest_path = "/mm_fixes";
    appine_kernel::write_file(&store, guest_path, &bytes, 0o755, true, 0, 0).unwrap();

    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--rootfs"])
            .arg(&store)
            .args(["--tier", tier, guest_path])
            .output()
            .expect("failed to run appine-run");
        let stdout = std::str::from_utf8(&out.stdout).unwrap();
        let stderr = std::str::from_utf8(&out.stderr).unwrap();
        assert!(
            out.status.success()
                && stdout.ends_with("all ok\n")
                && !stderr.contains("appine: internal error"),
            "tier {tier}: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            out.status,
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
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let tmp = dir.join(format!("guest-{name}.{}", std::process::id()));
    let out = dir.join(format!("guest-{name}"));
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
