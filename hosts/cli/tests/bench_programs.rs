//! M2 基准程序的正确性：原生、Tier 0、Tier 1 三种方式运行，输出必须逐字相同。
//!
//! 这里只用很小的轮数检查正确性，耗时对比见 `appine-bench`。需要 Linux aarch64 宿主。
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::path::Path;
use std::process::{Command, Output};

use appine_bench::{PROGRAMS, build};

fn check(output: Output, what: &str) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{what} failed ({}): {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn benchmarks_agree_across_tiers() {
    let out_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("bench-programs");
    std::fs::create_dir_all(&out_dir).unwrap();
    for program in PROGRAMS {
        let exe = build(program, &out_dir).unwrap_or_else(|e| panic!("{e}"));
        let rounds = program.test_rounds.to_string();
        let native = check(
            Command::new(&exe).arg(&rounds).output().unwrap(),
            &format!("{} (native)", program.name),
        );
        for tier in ["0", "1"] {
            let output = Command::new(env!("CARGO_BIN_EXE_appine-run"))
                .args(["--tier", tier])
                .arg(&exe)
                .arg(&rounds)
                .output()
                .unwrap();
            let got = check(output, &format!("{} (tier {tier})", program.name));
            assert_eq!(
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&native),
                "{} under tier {tier}",
                program.name
            );
        }
    }
}
