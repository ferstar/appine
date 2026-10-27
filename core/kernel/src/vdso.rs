//! vDSO（设计文档 §7.2）：一个最小但真实的 ELF 共享对象，在运行时按字节拼出来。
//!
//! 导出 `LINUX_2.6.39` 版本的四个符号，与 arm64 Linux 相同：
//!
//! - `__kernel_rt_sigreturn` 恰好是 `mov x8, #139; svc #0`，前面有一条 nop
//!   （libgcc 的 unwinder 靠逐字节匹配这两条指令识别信号帧，Linux 的 vDSO 也在它
//!   前面放了一条 nop）；
//! - `__kernel_clock_gettime`、`__kernel_gettimeofday`、`__kernel_clock_getres`
//!   在 v1 里是 svc 存根。
//!
//! 动态段提供 `DT_HASH`、`DT_SYMTAB`、`DT_STRTAB`、`DT_VERSYM`、`DT_VERDEF`，
//! 满足 musl、glibc 和 Go runtime 的 vDSO 解析器。每个进程映射一份私有拷贝。

use std::sync::OnceLock;

use appine_engine::PAGE_SIZE;

/// 拼好的镜像和其中 sigreturn 跳板的偏移。
struct Image {
    bytes: Vec<u8>,
    sigreturn: u64,
}

const VERSION: &[u8] = b"LINUX_2.6.39";
const SONAME: &[u8] = b"linux-vdso.so.1";

const NOP: u32 = 0xd503_201f;
const SVC0: u32 = 0xd400_0001;
const RET: u32 = 0xd65f_03c0;

/// `movz x8, #nr`。
const fn mov_x8(nr: u32) -> u32 {
    0xd280_0008 | (nr << 5)
}

/// System V ABI 的 ELF 哈希。
fn elf_hash(name: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &c in name {
        h = (h << 4).wrapping_add(c as u32);
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn align(b: &mut Vec<u8>, to: usize) {
    b.resize(b.len().next_multiple_of(to), 0);
}

fn build() -> Image {
    // 代码：sigreturn 前面的 nop 不属于任何符号
    let functions: [(&[u8], &[u32]); 4] = [
        (b"__kernel_rt_sigreturn", &[mov_x8(139), SVC0]),
        (b"__kernel_clock_gettime", &[mov_x8(113), SVC0, RET]),
        (b"__kernel_gettimeofday", &[mov_x8(169), SVC0, RET]),
        (b"__kernel_clock_getres", &[mov_x8(114), SVC0, RET]),
    ];
    let nsyms = functions.len() + 1;

    // 字符串表
    let mut strtab = vec![0u8];
    let mut add_str = |s: &[u8]| {
        let off = strtab.len() as u32;
        strtab.extend_from_slice(s);
        strtab.push(0);
        off
    };
    let name_offs: Vec<u32> = functions.iter().map(|(n, _)| add_str(n)).collect();
    let version_name = add_str(VERSION);
    let soname = add_str(SONAME);

    // 固定的头部：ELF 头、两个程序头（PT_LOAD、PT_DYNAMIC）
    const EHDR: usize = 64;
    const PHDR: usize = 56;
    let mut b = vec![0u8; EHDR + 2 * PHDR];

    // .hash：一个桶，所有符号串成一条链
    let hash = b.len();
    put32(&mut b, 1);
    put32(&mut b, nsyms as u32);
    put32(&mut b, 1);
    for i in 0..nsyms {
        let next = if i == 0 || i + 1 == nsyms { 0 } else { i + 1 };
        put32(&mut b, next as u32);
    }

    // .dynsym：先占位，代码的地址最后填
    align(&mut b, 8);
    let symtab = b.len();
    b.resize(symtab + nsyms * 24, 0);

    // .gnu.version：符号 0 是局部的，其余都属于版本 2（LINUX_2.6.39）
    let versym = b.len();
    put16(&mut b, 0);
    for _ in &functions {
        put16(&mut b, 2);
    }

    // .gnu.version_d：版本 1 是 soname（VER_FLG_BASE），版本 2 是 LINUX_2.6.39
    align(&mut b, 8);
    let verdef = b.len();
    for (ndx, flags, name, last) in [
        (1u16, 1u16, (SONAME, soname), false),
        (2, 0, (VERSION, version_name), true),
    ] {
        put16(&mut b, 1); // vd_version
        put16(&mut b, flags);
        put16(&mut b, ndx);
        put16(&mut b, 1); // vd_cnt
        put32(&mut b, elf_hash(name.0));
        put32(&mut b, 20); // vd_aux：紧跟在 Verdef 后面
        put32(&mut b, if last { 0 } else { 28 }); // vd_next
        put32(&mut b, name.1); // vda_name
        put32(&mut b, 0); // vda_next
    }

    let strtab_off = b.len();
    b.extend_from_slice(&strtab);

    align(&mut b, 8);
    let dynamic = b.len();
    let dyn_entries: [(u64, u64); 10] = [
        (4, hash as u64),             // DT_HASH
        (5, strtab_off as u64),       // DT_STRTAB
        (6, symtab as u64),           // DT_SYMTAB
        (10, strtab.len() as u64),    // DT_STRSZ
        (11, 24),                     // DT_SYMENT
        (14, soname as u64),          // DT_SONAME
        (0x6fff_fff0, versym as u64), // DT_VERSYM
        (0x6fff_fffc, verdef as u64), // DT_VERDEF
        (0x6fff_fffd, 2),             // DT_VERDEFNUM
        (0, 0),                       // DT_NULL
    ];
    for (tag, val) in dyn_entries {
        put64(&mut b, tag);
        put64(&mut b, val);
    }
    let dynamic_len = b.len() - dynamic;

    // .text
    align(&mut b, 16);
    put32(&mut b, NOP);
    let mut addrs = Vec::new();
    for (_, code) in &functions {
        addrs.push((b.len() as u64, code.len() as u64 * 4));
        for &insn in *code {
            put32(&mut b, insn);
        }
    }
    let size = b.len() as u64;
    assert!(size <= PAGE_SIZE, "vDSO does not fit in one page");

    // 回填符号表
    for (i, (&name, &(value, len))) in name_offs.iter().zip(&addrs).enumerate() {
        let at = symtab + (i + 1) * 24;
        let mut sym = Vec::with_capacity(24);
        put32(&mut sym, name);
        sym.push((1 << 4) | 2); // STB_GLOBAL, STT_FUNC
        sym.push(0);
        // 没有节头表；解析器只要求 st_shndx 不是 SHN_UNDEF
        put16(&mut sym, 1);
        put64(&mut sym, value);
        put64(&mut sym, len);
        b[at..at + 24].copy_from_slice(&sym);
    }

    // ELF 头
    let mut h = Vec::with_capacity(EHDR + 2 * PHDR);
    h.extend_from_slice(b"\x7fELF\x02\x01\x01");
    h.resize(16, 0);
    put16(&mut h, 3); // ET_DYN
    put16(&mut h, 183); // EM_AARCH64
    put32(&mut h, 1);
    put64(&mut h, 0); // e_entry
    put64(&mut h, EHDR as u64); // e_phoff
    put64(&mut h, 0); // e_shoff
    put32(&mut h, 0); // e_flags
    put16(&mut h, EHDR as u16);
    put16(&mut h, PHDR as u16);
    put16(&mut h, 2); // e_phnum
    put16(&mut h, 64); // e_shentsize
    put16(&mut h, 0); // e_shnum
    put16(&mut h, 0); // e_shstrndx
    // PT_LOAD：整个镜像，可读可执行
    put32(&mut h, 1);
    put32(&mut h, 5);
    for v in [0, 0, 0, size, size, PAGE_SIZE] {
        put64(&mut h, v);
    }
    // PT_DYNAMIC
    put32(&mut h, 2);
    put32(&mut h, 4);
    for v in [dynamic as u64, dynamic as u64, dynamic as u64] {
        put64(&mut h, v);
    }
    put64(&mut h, dynamic_len as u64);
    put64(&mut h, dynamic_len as u64);
    put64(&mut h, 8);
    b[..h.len()].copy_from_slice(&h);

    Image {
        bytes: b,
        sigreturn: addrs[0].0,
    }
}

fn get() -> &'static Image {
    static IMAGE: OnceLock<Image> = OnceLock::new();
    IMAGE.get_or_init(build)
}

/// vDSO 的镜像（不超过一页）。
pub fn image() -> &'static [u8] {
    &get().bytes
}

/// `__kernel_rt_sigreturn` 相对 vDSO 起点的偏移。
pub fn sigreturn_offset() -> u64 {
    get().sigreturn
}

/// svc 之后的 pc 是 `pc`、编号是 `nr` 的系统调用来自 vDSO 里读时钟的存根（clock_gettime、
/// clock_getres、gettimeofday）：Linux 上它们不进内核，跟踪者看不到，seccomp 也不检查。
pub(crate) fn clock_stub(cx: &crate::kernel::Cx<'_>, pc: u64, nr: u64) -> bool {
    clock_stub_at(cx.mm().vdso, pc, nr)
}

/// 与 [`clock_stub`] 相同，vDSO 映射在 `vdso`（不持 BKL 时用记下的地址）。
pub(crate) fn clock_stub_at(vdso: u64, pc: u64, nr: u64) -> bool {
    let svc = pc.wrapping_sub(4);
    vdso != 0 && (vdso..vdso + image().len() as u64).contains(&svc) && matches!(nr, 113 | 114 | 169)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
    }

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }

    fn cstr(b: &[u8], off: usize) -> &[u8] {
        let end = b[off..].iter().position(|&c| c == 0).unwrap();
        &b[off..off + end]
    }

    /// 按 musl `__vdsosym` 的做法查找符号（src/internal/vdso.c）。
    fn musl_lookup(b: &[u8], vername: &[u8], name: &[u8]) -> Option<u64> {
        let phoff = u64_at(b, 32) as usize;
        let mut dynv = None;
        let mut base = None;
        for i in 0..u16_at(b, 56) as usize {
            let ph = phoff + i * u16_at(b, 54) as usize;
            match u32_at(b, ph) {
                1 => base = Some(u64_at(b, ph + 8) - u64_at(b, ph + 16)),
                2 => dynv = Some(u64_at(b, ph + 8) as usize),
                _ => {}
            }
        }
        let (dynv, base) = (dynv?, base? as usize);
        let (mut strings, mut syms, mut hashtab, mut versym, mut verdef) = (0, 0, 0, 0, 0);
        let mut i = dynv;
        while u64_at(b, i) != 0 {
            let p = base + u64_at(b, i + 8) as usize;
            match u64_at(b, i) {
                5 => strings = p,
                6 => syms = p,
                4 => hashtab = p,
                0x6fff_fff0 => versym = p,
                0x6fff_fffc => verdef = p,
                _ => {}
            }
            i += 16;
        }
        assert!(strings != 0 && syms != 0 && hashtab != 0 && versym != 0 && verdef != 0);
        for i in 0..u32_at(b, hashtab + 4) as usize {
            let sym = syms + i * 24;
            let info = b[sym + 4];
            if (1 << (info & 0xf)) & 0b111 == 0 || (1 << (info >> 4)) & 0b110 == 0 {
                continue;
            }
            if u16_at(b, sym + 6) == 0 || cstr(b, strings + u32_at(b, sym) as usize) != name {
                continue;
            }
            // checkver
            let vsym = u16_at(b, versym + 2 * i) & 0x7fff;
            let mut def = verdef;
            let found = loop {
                if u16_at(b, def + 2) & 1 == 0 && u16_at(b, def + 4) & 0x7fff == vsym {
                    break true;
                }
                if u32_at(b, def + 16) == 0 {
                    break false;
                }
                def += u32_at(b, def + 16) as usize;
            };
            let aux = def + u32_at(b, def + 12) as usize;
            if found && cstr(b, strings + u32_at(b, aux) as usize) == vername {
                return Some(base as u64 + u64_at(b, sym + 8));
            }
        }
        None
    }

    #[test]
    fn musl_finds_the_exported_symbols() {
        let b = image();
        let sigreturn = musl_lookup(b, VERSION, b"__kernel_rt_sigreturn").unwrap();
        assert_eq!(sigreturn, sigreturn_offset());
        let at = sigreturn as usize;
        assert_eq!(u32_at(b, at - 4), NOP);
        assert_eq!(u32_at(b, at), 0xd280_1168); // mov x8, #139
        assert_eq!(u32_at(b, at + 4), SVC0);
        let gettime = musl_lookup(b, VERSION, b"__kernel_clock_gettime").unwrap() as usize;
        assert_eq!(u32_at(b, gettime), mov_x8(113));
        assert!(musl_lookup(b, b"LINUX_2.6", b"__kernel_clock_gettime").is_none());
        assert!(musl_lookup(b, VERSION, b"__kernel_time").is_none());
    }
}
