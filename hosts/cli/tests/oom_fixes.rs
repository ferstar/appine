//! Standalone integration tests for kernel/oom victim reaping.
//!
//! Runs `tests/guest/oom_reap.c` statically compiled with `musl-gcc` inside an empty rootfs under
//! both Tier 0 and Tier 1.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::process::Command;

const TIERS: [&str; 2] = ["0", "1"];

#[test]
fn oom_victim_reaping_and_vfork_safety() {
    let Some(program) = build_c_guest("oom_reap") else {
        return;
    };
    let bytes = std::fs::read(&program).unwrap();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let pid = std::process::id();
    let store = dir.join(format!("oom_reap-store.{pid}"));
    let empty_dir = dir.join(format!("oom_reap-empty.{pid}"));
    let tar_path = dir.join(format!("oom_reap-empty.{pid}.tar.gz"));
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&empty_dir);
    let _ = std::fs::remove_file(&tar_path);
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
    appine_kernel::write_file(&store, "/oom_reap", &bytes, 0o755, true, 0, 0).unwrap();

    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--rootfs"])
            .arg(&store)
            .args(["--tier", tier, "/oom_reap"])
            .output()
            .expect("failed to run appine-run");
        let stdout = std::str::from_utf8(&out.stdout).unwrap();
        let stderr = std::str::from_utf8(&out.stderr).unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "tier {tier}: stdout={stdout} stderr={stderr}"
        );
        assert_eq!(stdout, "PASS\n", "tier {tier}: stderr={stderr}");
        assert!(
            !stderr.contains("appine: internal error"),
            "tier {tier}: unexpected internal error: {stderr}"
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
    let out = dir.join(format!("guest-{name}"));
    let tmp = dir.join(format!("guest-{name}.{}", std::process::id()));
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
