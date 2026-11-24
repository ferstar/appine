//! Standalone guest integration tests for resumable netlink dumps (`kernel/netlink`) and chunked
//! nftables set-element dumps (`kernel/nft`), run under both Tier 0 and Tier 1.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::process::Command;

const TIERS: [&str; 2] = ["0", "1"];

#[test]
fn netlink_resumable_and_nft_setelem_dumps() {
    let Some(program) = build_c_guest("netlink_dump") else {
        return;
    };
    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--tier", tier])
            .arg(&program)
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
