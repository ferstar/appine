//! 原生执行端只能在 Linux aarch64 宿主上编译；其他宿主上 lockstep 测试自动跳过。

fn main() {
    println!("cargo::rustc-check-cfg=cfg(lockstep_native)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    if os != "linux" || arch != "aarch64" {
        return;
    }
    println!("cargo::rerun-if-changed=src/oracle.c");
    println!("cargo::rerun-if-changed=src/oracle.S");
    cc::Build::new()
        .file("src/oracle.c")
        .file("src/oracle.S")
        .std("c17")
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("lockstep_oracle");
    println!("cargo::rustc-cfg=lockstep_native");
}
