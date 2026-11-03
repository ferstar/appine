//! Tier 0 的指令白名单与 LLVM 反汇编器逐条对照（engine 的 map）。
//!
//! 需要 Linux aarch64 宿主和 llvm-mc（`APPINE_LLVM_MC` 或 PATH 中的 llvm-mc），
//! 缺少任何一个时跳过。样本数可以用 `APPINE_WHITELIST_SAMPLES` 调整。

#[cfg(lockstep_native)]
#[test]
fn whitelist_matches_llvm() {
    use appine_lockstep::whitelist::{check, find_llvm_mc};

    let Some(llvm_mc) = find_llvm_mc() else {
        eprintln!("skipped: llvm-mc not found");
        return;
    };
    let samples = std::env::var("APPINE_WHITELIST_SAMPLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    eprintln!("whitelist seed = {seed}");
    let (reports, found) = check(&llvm_mc, samples, seed, 40);
    for r in &reports {
        eprintln!(
            "  {:<12} {} samples, {} executable",
            r.space, r.samples, r.defined
        );
        assert!(
            r.defined > 0 || !r.available,
            "no executable samples in {}",
            r.space
        );
    }
    if !found.is_empty() {
        let lines: Vec<String> = found
            .iter()
            .map(|d| {
                format!(
                    "  [{}] {:#010x} tier0={} llvm={}  {}",
                    d.space, d.insn, d.tier0, d.llvm, d.text
                )
            })
            .collect();
        panic!(
            "tier 0 whitelist disagrees with llvm-mc (seed {seed}):\n{}",
            lines.join("\n")
        );
    }
}
