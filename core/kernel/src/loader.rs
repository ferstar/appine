//! ELF 加载器。
//!
//! M0 只支持静态链接、非 PIE（`ET_EXEC`）的 aarch64 程序，初始栈只带最小的
//! auxv；PIE、动态链接和完整 auxv 在 M1 加入（设计文档 §14）。

use std::collections::BTreeMap;
use std::fmt;

use appine_engine::{PAGE_SIZE, VA_BITS, prot};

use crate::errno::Errno;
use crate::mm::Mm;

const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const PHDR_SIZE: u64 = 56;

const AT_NULL: u64 = 0;
const AT_PAGESZ: u64 = 6;
const AT_ENTRY: u64 = 9;

/// 初始栈顶（不含），即 47 位地址空间的上界。
pub const STACK_TOP: u64 = 1 << 47;
/// 初始栈大小。
pub const STACK_SIZE: u64 = 8 << 20;
/// argv、envp 及其字符串最多占用的栈空间，超过时报 `E2BIG`。
const MAX_ARG_BYTES: usize = (STACK_SIZE / 4) as usize;

/// 加载失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// 不是 ELF 文件。
    NotElf,
    /// ELF 合法，但 M0 还不支持。
    Unsupported(&'static str),
    /// ELF 结构损坏。
    Malformed(&'static str),
    /// 参数和环境变量太多。
    ArgumentsTooLong,
    /// 建立映射失败。
    Memory(Errno),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NotElf => f.write_str("not an ELF file"),
            LoadError::Unsupported(what) => write!(f, "unsupported executable: {what}"),
            LoadError::Malformed(what) => write!(f, "malformed ELF: {what}"),
            LoadError::ArgumentsTooLong => f.write_str("argument list too long"),
            LoadError::Memory(errno) => write!(f, "cannot map memory: {errno}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// 加载结果：guest 开始执行时的 pc 和 sp。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image {
    /// 入口地址。
    pub entry: u64,
    /// 初始栈指针，指向 argc。
    pub sp: u64,
}

struct Segment {
    vaddr: u64,
    offset: usize,
    filesz: usize,
}

/// 把 ELF 文件 `file` 加载进 `mm`，并按 Linux 的约定构造初始栈。
pub fn load(
    mm: &mut Mm,
    file: &[u8],
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
) -> Result<Image, LoadError> {
    if file.len() < 64 || &file[..4] != b"\x7fELF" {
        return Err(LoadError::NotElf);
    }
    if file[4] != 2 || file[5] != 1 {
        return Err(LoadError::Unsupported("not a 64-bit little-endian ELF"));
    }
    if u16_at(file, 18)? != EM_AARCH64 {
        return Err(LoadError::Unsupported("not an aarch64 executable"));
    }
    match u16_at(file, 16)? {
        ET_EXEC => {}
        ET_DYN => {
            return Err(LoadError::Unsupported(
                "position-independent executables (M1)",
            ));
        }
        _ => return Err(LoadError::Unsupported("not an executable")),
    }
    let entry = u64_at(file, 24)?;
    let phoff = u64_at(file, 32)?;
    if u16_at(file, 54)? as u64 != PHDR_SIZE {
        return Err(LoadError::Malformed("unexpected program header size"));
    }
    let phnum = u16_at(file, 56)? as u64;

    // 每个 guest 页的权限是覆盖它的所有段的并集
    let mut pages: BTreeMap<u64, u32> = BTreeMap::new();
    let mut segments = Vec::new();
    for i in 0..phnum {
        let ph = phoff
            .checked_add(i * PHDR_SIZE)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or(LoadError::Malformed("program header out of range"))?;
        match u32_at(file, ph)? {
            PT_LOAD => {}
            PT_INTERP => {
                return Err(LoadError::Unsupported(
                    "dynamically linked executables (M1)",
                ));
            }
            _ => continue,
        }
        let flags = u32_at(file, ph + 4)?;
        let offset = u64_at(file, ph + 8)?;
        let vaddr = u64_at(file, ph + 16)?;
        let filesz = u64_at(file, ph + 32)?;
        let memsz = u64_at(file, ph + 40)?;
        if filesz > memsz {
            return Err(LoadError::Malformed(
                "segment file size exceeds memory size",
            ));
        }
        let file_end = offset.checked_add(filesz);
        if file_end.is_none_or(|end| end > file.len() as u64) {
            return Err(LoadError::Malformed("segment exceeds the file"));
        }
        let end = vaddr
            .checked_add(memsz)
            .filter(|&end| end <= 1 << VA_BITS)
            .ok_or(LoadError::Malformed("segment outside the address space"))?;
        if end > STACK_TOP - STACK_SIZE {
            return Err(LoadError::Unsupported("segment overlaps the stack"));
        }
        if memsz == 0 {
            continue;
        }
        let seg_prot = to_prot(flags);
        let mut page = vaddr & !(PAGE_SIZE - 1);
        while page < end {
            *pages.entry(page).or_insert(0) |= seg_prot;
            page += PAGE_SIZE;
        }
        segments.push(Segment {
            vaddr,
            offset: offset as usize,
            filesz: filesz as usize,
        });
    }
    if segments.is_empty() {
        return Err(LoadError::Malformed("no loadable segments"));
    }

    // 相邻且权限相同的页合并成一次映射
    let mut runs: Vec<(u64, u64, u32)> = Vec::new();
    for (&page, &page_prot) in &pages {
        match runs.last_mut() {
            Some((start, len, p)) if *start + *len == page && *p == page_prot => *len += PAGE_SIZE,
            _ => runs.push((page, PAGE_SIZE, page_prot)),
        }
    }
    for (start, len, run_prot) in runs {
        mm.map_anonymous(start, len, run_prot)
            .map_err(LoadError::Memory)?;
    }
    for seg in &segments {
        mm.write_kernel(seg.vaddr, &file[seg.offset..seg.offset + seg.filesz])
            .map_err(LoadError::Memory)?;
    }

    mm.map_anonymous(STACK_TOP - STACK_SIZE, STACK_SIZE, prot::READ | prot::WRITE)
        .map_err(LoadError::Memory)?;
    let sp = build_stack(mm, argv, envp, entry)?;
    Ok(Image { entry, sp })
}

/// 按 Linux 的布局构造初始栈：
///
/// ```text
/// sp → argc | argv[0..n] | NULL | envp[0..m] | NULL | auxv 键值对 | AT_NULL
///      ……
///      argv 与 envp 的字符串（升序排列）
/// STACK_TOP
/// ```
fn build_stack(mm: &Mm, argv: &[Vec<u8>], envp: &[Vec<u8>], entry: u64) -> Result<u64, LoadError> {
    let strings: usize = argv.iter().chain(envp).map(|s| s.len() + 1).sum();
    let words = 1 + argv.len() + 1 + envp.len() + 1 + 2 * 3;
    if strings + words * 8 > MAX_ARG_BYTES {
        return Err(LoadError::ArgumentsTooLong);
    }

    let mut cursor = (STACK_TOP - 16 - strings as u64) & !15;
    let mut table = vec![argv.len() as u64];
    for (i, s) in argv.iter().chain(envp).enumerate() {
        if i == argv.len() {
            table.push(0);
        }
        mm.write_kernel(cursor, s).map_err(LoadError::Memory)?;
        mm.write_kernel(cursor + s.len() as u64, &[0])
            .map_err(LoadError::Memory)?;
        table.push(cursor);
        cursor += s.len() as u64 + 1;
    }
    if envp.is_empty() {
        table.push(0);
    }
    table.push(0);
    table.extend([AT_PAGESZ, PAGE_SIZE, AT_ENTRY, entry, AT_NULL, 0]);

    let strings_start = (STACK_TOP - 16 - strings as u64) & !15;
    let sp = (strings_start - table.len() as u64 * 8) & !15;
    let bytes: Vec<u8> = table.iter().flat_map(|w| w.to_le_bytes()).collect();
    mm.write_kernel(sp, &bytes).map_err(LoadError::Memory)?;
    Ok(sp)
}

fn to_prot(flags: u32) -> u32 {
    let mut p = 0;
    if flags & PF_R != 0 {
        p |= prot::READ;
    }
    if flags & PF_W != 0 {
        // arm64 Linux 没有只写页：可写意味着可读
        p |= prot::READ | prot::WRITE;
    }
    if flags & PF_X != 0 {
        p |= prot::EXEC;
    }
    p
}

fn bytes_at<const N: usize>(file: &[u8], off: usize) -> Result<[u8; N], LoadError> {
    file.get(
        off..off
            .checked_add(N)
            .ok_or(LoadError::Malformed("truncated"))?,
    )
    .map(|b| b.try_into().unwrap())
    .ok_or(LoadError::Malformed("truncated"))
}

fn u16_at(file: &[u8], off: usize) -> Result<u16, LoadError> {
    bytes_at(file, off).map(u16::from_le_bytes)
}

fn u32_at(file: &[u8], off: usize) -> Result<u32, LoadError> {
    bytes_at(file, off).map(u32::from_le_bytes)
}

fn u64_at(file: &[u8], off: usize) -> Result<u64, LoadError> {
    bytes_at(file, off).map(u64::from_le_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uaccess::UserSlice;

    /// 构造一个只有一个 PT_LOAD 段的最小 ELF，代码放在 0x400000 + 0x78。
    fn tiny_elf(e_type: u16, code: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 0x78];
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = 2;
        f[5] = 1;
        f[6] = 1;
        f[16..18].copy_from_slice(&e_type.to_le_bytes());
        f[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());
        f[24..32].copy_from_slice(&0x400078u64.to_le_bytes());
        f[32..40].copy_from_slice(&64u64.to_le_bytes());
        f[54..56].copy_from_slice(&56u16.to_le_bytes());
        f[56..58].copy_from_slice(&1u16.to_le_bytes());
        let ph = 64;
        f[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        f[ph + 4..ph + 8].copy_from_slice(&(PF_R | PF_X).to_le_bytes());
        f[ph + 16..ph + 24].copy_from_slice(&0x400000u64.to_le_bytes());
        let size = (0x78 + code.len()) as u64;
        f[ph + 32..ph + 40].copy_from_slice(&size.to_le_bytes());
        f[ph + 40..ph + 48].copy_from_slice(&size.to_le_bytes());
        f.extend_from_slice(code);
        f
    }

    fn read_u64(mm: &Mm, addr: u64) -> u64 {
        let mut b = [0u8; 8];
        mm.read(UserSlice::new(addr, 8), &mut b).unwrap();
        u64::from_le_bytes(b)
    }

    fn read_cstr(mm: &Mm, addr: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = [0u8; 1];
            mm.read(UserSlice::new(addr + out.len() as u64, 1), &mut b)
                .unwrap();
            if b[0] == 0 {
                return out;
            }
            out.push(b[0]);
        }
    }

    #[test]
    fn loads_segment_and_builds_stack() {
        let mut mm = Mm::new();
        let code = 0xd503201fu32.to_le_bytes(); // nop
        let argv = vec![b"prog".to_vec(), b"arg1".to_vec()];
        let envp = vec![b"K=V".to_vec()];
        let image = load(&mut mm, &tiny_elf(ET_EXEC, &code), &argv, &envp).unwrap();
        assert_eq!(image.entry, 0x400078);
        assert_eq!(image.sp % 16, 0);
        assert_eq!(read_u64(&mm, 0x400078) as u32, 0xd503201f);
        assert!(mm.mmu().translate(0x400000, prot::EXEC).is_some());
        assert!(mm.mmu().translate(0x400000, prot::WRITE).is_none());

        let sp = image.sp;
        assert_eq!(read_u64(&mm, sp), 2);
        assert_eq!(read_cstr(&mm, read_u64(&mm, sp + 8)), b"prog");
        assert_eq!(read_cstr(&mm, read_u64(&mm, sp + 16)), b"arg1");
        assert_eq!(read_u64(&mm, sp + 24), 0);
        assert_eq!(read_cstr(&mm, read_u64(&mm, sp + 32)), b"K=V");
        assert_eq!(read_u64(&mm, sp + 40), 0);
        assert_eq!(read_u64(&mm, sp + 48), AT_PAGESZ);
        assert_eq!(read_u64(&mm, sp + 56), PAGE_SIZE);
        assert_eq!(read_u64(&mm, sp + 64), AT_ENTRY);
        assert_eq!(read_u64(&mm, sp + 72), 0x400078);
        assert_eq!(read_u64(&mm, sp + 80), AT_NULL);
    }

    #[test]
    fn rejects_unsupported_files() {
        let mut mm = Mm::new();
        assert_eq!(
            load(&mut mm, b"#!/bin/sh\n", &[], &[]),
            Err(LoadError::NotElf)
        );
        assert!(matches!(
            load(&mut mm, &tiny_elf(ET_DYN, &[0; 4]), &[], &[]),
            Err(LoadError::Unsupported(_))
        ));
    }
}
