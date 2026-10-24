//! 编译 C 引擎（设计文档 §13.1：引擎用 C17 + aarch64 汇编）。

fn main() {
    let sources = ["src/c/cpu.c", "src/c/interp.c", "src/c/mmu.c"];
    for path in sources
        .iter()
        .chain(&["src/c/internal.h", "include/appine/engine.h"])
    {
        println!("cargo::rerun-if-changed={path}");
    }

    cc::Build::new()
        .files(sources)
        .include("include")
        .std("c17")
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("appine_engine");

    // 供依赖方的 C 代码引用头文件：环境变量 DEP_APPINE_ENGINE_INCLUDE
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let include = std::path::Path::new(&manifest_dir).join("include");
    println!("cargo::metadata=include={}", include.display());
}
