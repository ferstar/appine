//! With the `vt` feature, builds libghostty-vt (static, with Zig) and vt.c, the experimental
//! terminal view's emulator (src/vt.rs). Without it, or for a host other than Android (where the
//! crate is empty), does nothing.
//!
//! The Ghostty source comes from scripts/fetch-ghostty (target/ghostty/ghostty-<version>, or
//! APPINE_GHOSTTY_SRC), which also fetched the Zig packages, so this build needs no network. The
//! Zig binary is `zig` on PATH or ZIG; Ghostty's build finds the NDK through ANDROID_NDK_HOME,
//! which the Gradle task sets.

use std::env;
use std::path::PathBuf;
use std::process::Command;

const GHOSTTY: &str = "ghostty-1.4.0";

fn main() {
    println!("cargo:rerun-if-changed=src/vt.c");
    println!("cargo:rerun-if-env-changed=APPINE_GHOSTTY_SRC");
    println!("cargo:rerun-if-env-changed=ZIG");
    if env::var_os("CARGO_FEATURE_VT").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android")
    {
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let src = env::var_os("APPINE_GHOSTTY_SRC").map_or_else(
        || manifest.join("../../target/ghostty").join(GHOSTTY),
        PathBuf::from,
    );
    assert!(
        src.join("build.zig").is_file(),
        "no Ghostty source at {}: run scripts/fetch-ghostty",
        src.display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        src.join("build.zig.zon").display()
    );

    let target = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "aarch64-linux-android",
        other => panic!("libghostty-vt: unsupported Android architecture {other:?}"),
    };
    let prefix = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("ghostty");
    let zig = env::var_os("ZIG").unwrap_or_else(|| "zig".into());
    // Zig's HTTP client fails through some proxies (scripts/fetch-ghostty); with the packages
    // already fetched it should not need the network, but a missing one then fails clearly
    let status = Command::new(&zig)
        .current_dir(&src)
        .args([
            "build",
            "-Demit-lib-vt",
            "-Dcpu=baseline",
            "-Doptimize=ReleaseFast",
        ])
        .arg(format!("-Dtarget={target}"))
        .arg("--prefix")
        .arg(&prefix)
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .status()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", zig.to_string_lossy()));
    assert!(
        status.success(),
        "zig build of libghostty-vt failed: {status}"
    );

    cc::Build::new()
        .file("src/vt.c")
        .include(prefix.join("include"))
        .std("c11")
        .warnings(true)
        .extra_warnings(true)
        .compile("appine_vt");
    // After vt.c, which uses it
    println!(
        "cargo:rustc-link-search=native={}",
        prefix.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=ghostty-vt");
}
