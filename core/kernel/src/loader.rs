//! ELF 加载器（设计文档 §6.1）。
//!
//! 支持静态链接（`ET_EXEC`）和位置无关（`ET_DYN`）的 aarch64 程序。带
//! `PT_INTERP` 的程序同时加载解释器（Alpine 上是 `ld-musl`），从解释器的入口
//! 开始执行，由它完成动态链接。仓库里的程序以私有文件映射的方式映射各段，代码页
//! 经由宿主 page cache 在进程之间共享（设计文档 §5.2）；没有宿主文件的程序和段之间
//! 共用页的程序把内容拷贝进匿名内存。初始栈按 Linux 的约定构造，auxv 与 arm64 Linux
//! 给出的条目和顺序一致；vDSO 映射在解释器下方。

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

use appine_engine::{PAGE_SIZE, VA_BITS, prot};

use crate::errno::Errno;
use crate::mm::{Mm, flags};

const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const PHDR_SIZE: u64 = 56;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_FLAGS: u64 = 8;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_PLATFORM: u64 = 15;
const AT_HWCAP: u64 = 16;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_HWCAP2: u64 = 26;
const AT_EXECFN: u64 = 31;
const AT_SYSINFO_EHDR: u64 = 33;
const AT_MINSIGSTKSZ: u64 = 51;

/// 初始栈顶（不含），即 47 位地址空间的上界。
pub const STACK_TOP: u64 = 1 << 47;
/// 初始栈大小。
pub const STACK_SIZE: u64 = 8 << 20;
/// argv、envp 及其字符串最多占用的栈空间，超过时报 `E2BIG`。
const MAX_ARG_BYTES: usize = (STACK_SIZE / 4) as usize;
/// 位置无关的主程序的加载基址：地址空间的 2/3，与 arm64 Linux 的 `ELF_ET_DYN_BASE` 取法相同。
const ET_DYN_BASE: u64 = 0x5555_5555_0000;
/// 解释器路径的最大长度。
const MAX_INTERP_PATH: u64 = 4096;
/// `sysconf(_SC_CLK_TCK)`。
pub const CLOCK_TICKS: u64 = 100;

/// 加载失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// 不是 ELF 文件。
    NotElf,
    /// ELF 合法，但还不支持。
    Unsupported(&'static str),
    /// ELF 结构损坏。
    Malformed(&'static str),
    /// 参数和环境变量太多。
    ArgumentsTooLong,
    /// 建立映射失败。
    Memory(Errno),
    /// 读取解释器失败。
    Interpreter(Vec<u8>, Errno),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NotElf => f.write_str("not an ELF file"),
            LoadError::Unsupported(what) => write!(f, "unsupported executable: {what}"),
            LoadError::Malformed(what) => write!(f, "malformed ELF: {what}"),
            LoadError::ArgumentsTooLong => f.write_str("argument list too long"),
            LoadError::Memory(errno) => write!(f, "cannot map memory: {errno}"),
            LoadError::Interpreter(path, errno) => {
                write!(
                    f,
                    "cannot load interpreter {}: {errno}",
                    String::from_utf8_lossy(path)
                )
            }
        }
    }
}

impl std::error::Error for LoadError {}

/// guest 程序的启动参数。
#[derive(Debug, Clone, Copy)]
pub struct Args<'a> {
    /// argv。
    pub argv: &'a [Vec<u8>],
    /// envp。
    pub envp: &'a [Vec<u8>],
    /// 程序路径，放进 `AT_EXECFN`。
    pub execfn: &'a [u8],
    /// 新程序的真实和有效用户 ID、组 ID（`AT_UID` 等）。两组不同时（setuid、setgid
    /// 程序）设置 `AT_SECURE`。
    pub ids: [u32; 4],
}

/// 加载结果：guest 开始执行时的 pc 和 sp，以及程序和解释器所在的地址范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// 入口地址（有解释器时是解释器的入口）。
    pub entry: u64,
    /// 初始栈指针，指向 argc。
    pub sp: u64,
    /// 主程序映射的 `[起点, 终点)`。
    pub main: (u64, u64),
    /// 解释器的路径和映射的 `[起点, 终点)`。
    pub interp: Option<(Vec<u8>, u64, u64)>,
}

struct Segment {
    vaddr: u64,
    memsz: u64,
    offset: usize,
    filesz: usize,
    prot: u32,
}

/// 要加载的可执行文件：全部内容，以及能直接映射进 guest 地址空间的宿主文件。
pub struct Executable {
    bytes: Bytes,
    file: Option<File>,
}

enum Bytes {
    /// 宿主文件的只读映射：解析时按需从 page cache 读取，不必整个读进内存。
    Mapped {
        base: NonNull<u8>,
        len: usize,
        map_len: usize,
    },
    Owned(Vec<u8>),
}

impl Executable {
    /// 宿主文件（仓库里的 object）。
    pub fn open(file: File) -> Result<Executable, Errno> {
        let len = file
            .metadata()
            .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?
            .len() as usize;
        if len == 0 {
            return Ok(Executable::from_bytes(Vec::new()));
        }
        let m = appine_hal::map_file(file.as_raw_fd(), 0, len, true, false).map_err(Errno)?;
        Ok(Executable {
            bytes: Bytes::Mapped {
                base: m.base,
                len,
                map_len: m.len,
            },
            file: Some(file),
        })
    }

    /// 内存里的内容，没有宿主文件：各段拷贝进匿名内存。
    pub fn from_bytes(data: Vec<u8>) -> Executable {
        Executable {
            bytes: Bytes::Owned(data),
            file: None,
        }
    }

    /// 文件的全部内容。
    pub fn bytes(&self) -> &[u8] {
        match &self.bytes {
            // SAFETY: 映射在 Executable 析构之前一直有效
            Bytes::Mapped { base, len, .. } => unsafe {
                std::slice::from_raw_parts(base.as_ptr(), *len)
            },
            Bytes::Owned(v) => v,
        }
    }
}

impl Drop for Executable {
    fn drop(&mut self) {
        if let Bytes::Mapped { base, map_len, .. } = self.bytes {
            // SAFETY: 映射来自 open，bytes() 借出的切片不会活得比 self 长
            unsafe { appine_hal::unmap(base, map_len) };
        }
    }
}

/// 解析后的 ELF 文件。
struct Elf<'a> {
    file: &'a [u8],
    /// 可以直接映射的宿主文件。
    fd: Option<i32>,
    e_type: u16,
    entry: u64,
    phoff: u64,
    phnum: u64,
    phdr_vaddr: Option<u64>,
    interp: Option<Vec<u8>>,
    segments: Vec<Segment>,
}

/// 映射到内存后的 ELF。
struct Loaded {
    bias: u64,
    entry: u64,
    phdr: u64,
    phnum: u64,
    end: u64,
}

impl<'a> Elf<'a> {
    fn parse(exe: &'a Executable) -> Result<Self, LoadError> {
        let file = exe.bytes();
        if file.len() < 64 || &file[..4] != b"\x7fELF" {
            return Err(LoadError::NotElf);
        }
        if file[4] != 2 || file[5] != 1 {
            return Err(LoadError::Unsupported("not a 64-bit little-endian ELF"));
        }
        if u16_at(file, 18)? != EM_AARCH64 {
            return Err(LoadError::Unsupported("not an aarch64 executable"));
        }
        let e_type = u16_at(file, 16)?;
        if e_type != ET_EXEC && e_type != ET_DYN {
            return Err(LoadError::Unsupported("not an executable"));
        }
        let entry = u64_at(file, 24)?;
        let phoff = u64_at(file, 32)?;
        if u16_at(file, 54)? as u64 != PHDR_SIZE {
            return Err(LoadError::Malformed("unexpected program header size"));
        }
        let phnum = u16_at(file, 56)? as u64;

        let mut elf = Elf {
            file,
            fd: exe.file.as_ref().map(AsRawFd::as_raw_fd),
            e_type,
            entry,
            phoff,
            phnum,
            phdr_vaddr: None,
            interp: None,
            segments: Vec::new(),
        };
        for i in 0..phnum {
            let ph = phoff
                .checked_add(i * PHDR_SIZE)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or(LoadError::Malformed("program header out of range"))?;
            let p_type = u32_at(file, ph)?;
            let flags = u32_at(file, ph + 4)?;
            let offset = u64_at(file, ph + 8)?;
            let vaddr = u64_at(file, ph + 16)?;
            let filesz = u64_at(file, ph + 32)?;
            let memsz = u64_at(file, ph + 40)?;
            match p_type {
                PT_PHDR => elf.phdr_vaddr = Some(vaddr),
                PT_INTERP => {
                    let range = usize::try_from(offset)
                        .ok()
                        .zip(usize::try_from(filesz).ok())
                        .filter(|_| filesz > 1 && filesz <= MAX_INTERP_PATH)
                        .and_then(|(o, n)| file.get(o..o.checked_add(n)?))
                        .ok_or(LoadError::Malformed("bad interpreter path"))?;
                    let path = range
                        .strip_suffix(&[0])
                        .ok_or(LoadError::Malformed("bad interpreter path"))?;
                    if path.contains(&0) {
                        return Err(LoadError::Malformed("bad interpreter path"));
                    }
                    elf.interp = Some(path.to_vec());
                }
                PT_LOAD => {
                    if filesz > memsz {
                        return Err(LoadError::Malformed(
                            "segment file size exceeds memory size",
                        ));
                    }
                    // 只含 .bss 的段 filesz 为 0，p_offset 可以指到文件末尾之后，Linux 不会去读它
                    let file_end = offset.checked_add(filesz);
                    if filesz > 0 && file_end.is_none_or(|end| end > file.len() as u64) {
                        return Err(LoadError::Malformed("segment exceeds the file"));
                    }
                    if vaddr
                        .checked_add(memsz)
                        .is_none_or(|end| end > 1 << VA_BITS)
                    {
                        return Err(LoadError::Malformed("segment outside the address space"));
                    }
                    if memsz == 0 {
                        continue;
                    }
                    elf.segments.push(Segment {
                        vaddr,
                        memsz,
                        offset: offset as usize,
                        filesz: filesz as usize,
                        prot: to_prot(flags),
                    });
                }
                _ => {}
            }
        }
        if elf.segments.is_empty() {
            return Err(LoadError::Malformed("no loadable segments"));
        }
        Ok(elf)
    }

    /// 段覆盖的页范围 `[起点, 终点)`（未加偏移）。
    fn span(&self) -> (u64, u64) {
        let lo = self.segments.iter().map(|s| s.vaddr).min().unwrap() & !(PAGE_SIZE - 1);
        let hi = self
            .segments
            .iter()
            .map(|s| s.vaddr + s.memsz)
            .max()
            .unwrap();
        (lo, hi.next_multiple_of(PAGE_SIZE))
    }

    /// 程序头在内存中的地址（未加偏移）：优先用 `PT_PHDR`，否则找包含程序头的段。
    fn phdr_vaddr(&self) -> Result<u64, LoadError> {
        if let Some(v) = self.phdr_vaddr {
            return Ok(v);
        }
        let size = self.phnum * PHDR_SIZE;
        self.segments
            .iter()
            .find(|s| {
                s.offset as u64 <= self.phoff && self.phoff + size <= (s.offset + s.filesz) as u64
            })
            .map(|s| s.vaddr + (self.phoff - s.offset as u64))
            .ok_or(LoadError::Malformed("program headers are not loaded"))
    }

    /// 以偏移 `bias` 把段映射进 `mm`。
    fn map(&self, mm: &mut Mm, bias: u64) -> Result<Loaded, LoadError> {
        let (_, hi) = self.span();
        if bias
            .checked_add(hi)
            .is_none_or(|end| end > STACK_TOP - STACK_SIZE)
        {
            return Err(LoadError::Unsupported("segment overlaps the stack"));
        }
        // 每个 guest 页的权限是覆盖它的所有段的并集
        let mut pages: BTreeMap<u64, u32> = BTreeMap::new();
        let mut shared_page = false;
        for seg in &self.segments {
            let mut page = (bias + seg.vaddr) & !(PAGE_SIZE - 1);
            while page < bias + seg.vaddr + seg.memsz {
                match pages.get_mut(&page) {
                    Some(p) => {
                        *p |= seg.prot;
                        shared_page = true;
                    }
                    None => {
                        pages.insert(page, seg.prot);
                    }
                }
                page += PAGE_SIZE;
            }
        }
        let loaded = Loaded {
            bias,
            entry: bias + self.entry,
            phdr: bias + self.phdr_vaddr()?,
            phnum: self.phnum,
            end: bias + hi,
        };
        // 文件偏移和地址在页内的位置一致（Linux 也这样要求）并且段之间不共用页时，
        // 直接映射宿主文件
        let aligned = self
            .segments
            .iter()
            .all(|s| (s.offset as u64 ^ s.vaddr) & (PAGE_SIZE - 1) == 0);
        if let (Some(fd), false, true) = (self.fd, shared_page, aligned) {
            for seg in &self.segments {
                self.map_from_file(mm, bias, seg, fd)?;
            }
            return Ok(loaded);
        }
        // 相邻且权限相同的页合并成一次映射
        let mut runs: Vec<(u64, u64, u32)> = Vec::new();
        for (&page, &page_prot) in &pages {
            match runs.last_mut() {
                Some((start, len, p)) if *start + *len == page && *p == page_prot => {
                    *len += PAGE_SIZE
                }
                _ => runs.push((page, PAGE_SIZE, page_prot)),
            }
        }
        for (start, len, run_prot) in runs {
            mm.map_anonymous(start, len, run_prot)
                .map_err(LoadError::Memory)?;
        }
        for seg in self.segments.iter().filter(|seg| seg.filesz > 0) {
            mm.write_kernel(
                bias + seg.vaddr,
                &self.file[seg.offset..seg.offset + seg.filesz],
            )
            .map_err(LoadError::Memory)?;
        }
        Ok(loaded)
    }

    /// 以私有文件映射映射一个段：文件内容所在的页映射宿主文件，最后一页里文件内容之后
    /// 的部分清零（.bss 的开头），其余的 .bss 是匿名内存。
    fn map_from_file(
        &self,
        mm: &mut Mm,
        bias: u64,
        seg: &Segment,
        fd: i32,
    ) -> Result<(), LoadError> {
        let start = (bias + seg.vaddr) & !(PAGE_SIZE - 1);
        let data_end = bias + seg.vaddr + seg.filesz as u64;
        let end = (bias + seg.vaddr + seg.memsz).next_multiple_of(PAGE_SIZE);
        let mut anon_start = start;
        if seg.filesz > 0 {
            let file_end = data_end.next_multiple_of(PAGE_SIZE);
            let offset = seg.offset as u64 - (bias + seg.vaddr - start);
            let fixed = flags::MAP_PRIVATE | flags::MAP_FIXED;
            mm.mmap(
                start,
                file_end - start,
                seg.prot,
                fixed,
                Some((fd, offset, false)),
            )
            .map_err(LoadError::Memory)?;
            if seg.memsz > seg.filesz as u64 && file_end > data_end {
                let zeros = vec![0u8; (file_end - data_end) as usize];
                mm.write_kernel(data_end, &zeros)
                    .map_err(LoadError::Memory)?;
            }
            anon_start = file_end;
        }
        if end > anon_start {
            mm.map_anonymous(anon_start, end - anon_start, seg.prot)
                .map_err(LoadError::Memory)?;
        }
        Ok(())
    }
}

/// 按 guest 路径打开解释器文件的回调。
pub type ReadInterp<'a> = dyn FnMut(&[u8]) -> Result<Executable, Errno> + 'a;

/// 把 ELF 文件 `file` 加载进 `mm`，并按 Linux 的约定构造初始栈。
///
/// 程序带 `PT_INTERP` 时，用 `read_interp` 按 guest 路径读取解释器文件。
pub fn load(
    mm: &mut Mm,
    file: &Executable,
    args: &Args<'_>,
    read_interp: &mut ReadInterp<'_>,
) -> Result<Image, LoadError> {
    let exe = Elf::parse(file)?;
    let bias = match exe.e_type {
        ET_DYN => ET_DYN_BASE - exe.span().0,
        _ => 0,
    };
    let main = exe.map(mm, bias)?;

    let mut interp_base = 0;
    let mut interp_range = None;
    let mut entry = main.entry;
    if let Some(path) = &exe.interp {
        let data = read_interp(path).map_err(|e| LoadError::Interpreter(path.clone(), e))?;
        let interp = Elf::parse(&data)?;
        if interp.e_type != ET_DYN || interp.interp.is_some() {
            return Err(LoadError::Unsupported("interpreter is not a shared object"));
        }
        let (lo, hi) = interp.span();
        let base = mm
            .find_free(hi - lo)
            .ok_or(LoadError::Memory(Errno::ENOMEM))?;
        let loaded = interp.map(mm, base - lo)?;
        crate::debug::log(format_args!(
            "loaded {} at {:#x}, interpreter at {base:#x}",
            String::from_utf8_lossy(args.execfn),
            main.bias + exe.span().0
        ));
        interp_base = loaded.bias + lo;
        interp_range = Some((path.clone(), interp_base, loaded.end));
        entry = loaded.entry;
    }

    let vdso = crate::vdso::image();
    let vdso_len = (vdso.len() as u64).next_multiple_of(PAGE_SIZE);
    let vdso_base = mm
        .find_free(vdso_len)
        .ok_or(LoadError::Memory(Errno::ENOMEM))?;
    mm.map_anonymous(vdso_base, vdso_len, prot::READ | prot::EXEC)
        .and_then(|()| mm.write_kernel(vdso_base, vdso))
        .map_err(LoadError::Memory)?;
    mm.vdso = vdso_base;

    mm.set_brk(main.end);
    mm.map_anonymous(STACK_TOP - STACK_SIZE, STACK_SIZE, prot::READ | prot::WRITE)
        .map_err(LoadError::Memory)?;
    let mut random = [0u8; 16];
    appine_hal::fill_random(&mut random).map_err(|e| LoadError::Memory(Errno(e)))?;
    // arm64 的 ARCH_DLINFO（AT_SYSINFO_EHDR、AT_MINSIGSTKSZ）排在最前面
    let auxv = [
        (AT_SYSINFO_EHDR, vdso_base),
        (AT_MINSIGSTKSZ, crate::signal::MIN_SIGSTKSZ),
        (AT_HWCAP, appine_engine::guest_hwcap()),
        (AT_PAGESZ, PAGE_SIZE),
        (AT_CLKTCK, CLOCK_TICKS),
        (AT_PHDR, main.phdr),
        (AT_PHENT, PHDR_SIZE),
        (AT_PHNUM, main.phnum),
        (AT_BASE, interp_base),
        (AT_FLAGS, 0),
        (AT_ENTRY, main.entry),
        (AT_UID, args.ids[0] as u64),
        (AT_EUID, args.ids[1] as u64),
        (AT_GID, args.ids[2] as u64),
        (AT_EGID, args.ids[3] as u64),
        (
            AT_SECURE,
            (args.ids[0] != args.ids[1] || args.ids[2] != args.ids[3]) as u64,
        ),
        (AT_RANDOM, 0),
        (AT_HWCAP2, 0),
        (AT_EXECFN, 0),
        (AT_PLATFORM, 0),
    ];
    let StackLayout { sp, args, env } = build_stack(mm, args, &auxv, &random)?;
    mm.args = args;
    mm.env = env;
    Ok(Image {
        entry,
        sp,
        main: (main.bias + exe.span().0, main.end),
        interp: interp_range,
    })
}

/// 初始栈的布局。
struct StackLayout {
    /// 指向 argc 的栈指针。
    sp: u64,
    /// argv 字符串所在的范围。
    args: (u64, u64),
    /// envp 字符串所在的范围。
    env: (u64, u64),
}

/// 按 Linux 的布局构造初始栈：
///
/// ```text
/// sp → argc | argv[0..n] | NULL | envp[0..m] | NULL | auxv 键值对 | AT_NULL
///      ……
///      AT_RANDOM 的 16 字节 | argv、envp、AT_EXECFN、AT_PLATFORM 的字符串
/// STACK_TOP
/// ```
///
/// `auxv` 里 `AT_RANDOM`、`AT_EXECFN`、`AT_PLATFORM` 的值在这里填入。返回 sp，以及
/// argv、envp 字符串所在的范围。
fn build_stack(
    mm: &Mm,
    args: &Args<'_>,
    auxv: &[(u64, u64)],
    random: &[u8; 16],
) -> Result<StackLayout, LoadError> {
    const PLATFORM: &[u8] = b"aarch64";
    let (argv, envp) = (args.argv, args.envp);
    let extra = [args.execfn, PLATFORM];
    let strings: usize = argv.iter().chain(envp).map(|s| s.len() + 1).sum::<usize>()
        + extra.iter().map(|s| s.len() + 1).sum::<usize>()
        + random.len();
    let words = 1 + argv.len() + 1 + envp.len() + 1 + 2 * (auxv.len() + 1);
    if strings + words * 8 > MAX_ARG_BYTES {
        return Err(LoadError::ArgumentsTooLong);
    }

    let strings_start = (STACK_TOP - 16 - strings as u64) & !15;
    let mut cursor = strings_start;
    let mut put = |data: &[u8], nul: bool| -> Result<u64, LoadError> {
        let at = cursor;
        mm.write_kernel(at, data).map_err(LoadError::Memory)?;
        cursor += data.len() as u64;
        if nul {
            mm.write_kernel(cursor, &[0]).map_err(LoadError::Memory)?;
            cursor += 1;
        }
        Ok(at)
    };
    let random_at = put(random, false)?;
    let mut table = vec![argv.len() as u64];
    let args_start = random_at + random.len() as u64;
    for s in argv {
        table.push(put(s, true)?);
    }
    table.push(0);
    let env_start = args_start + argv.iter().map(|s| s.len() as u64 + 1).sum::<u64>();
    for s in envp {
        table.push(put(s, true)?);
    }
    table.push(0);
    let env_end = env_start + envp.iter().map(|s| s.len() as u64 + 1).sum::<u64>();
    let execfn_at = put(args.execfn, true)?;
    let platform_at = put(PLATFORM, true)?;
    for &(key, value) in auxv {
        let value = match key {
            AT_RANDOM => random_at,
            AT_EXECFN => execfn_at,
            AT_PLATFORM => platform_at,
            _ => value,
        };
        table.extend([key, value]);
    }
    table.extend([AT_NULL, 0]);

    let sp = (strings_start - table.len() as u64 * 8) & !15;
    let bytes: Vec<u8> = table.iter().flat_map(|w| w.to_le_bytes()).collect();
    mm.write_kernel(sp, &bytes).map_err(LoadError::Memory)?;
    Ok(StackLayout {
        sp,
        args: (args_start, env_start),
        env: (env_start, env_end),
    })
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

    /// 在 `tiny_elf` 后面追加一个 PT_INTERP 程序头和路径。
    fn with_interp(mut f: Vec<u8>, path: &[u8]) -> Vec<u8> {
        let code = f.split_off(0x78);
        f.truncate(64 + 56);
        f[56..58].copy_from_slice(&2u16.to_le_bytes());
        let path_at = 64 + 2 * 56;
        let mut ph = vec![0u8; 56];
        ph[..4].copy_from_slice(&PT_INTERP.to_le_bytes());
        ph[8..16].copy_from_slice(&(path_at as u64).to_le_bytes());
        ph[32..40].copy_from_slice(&(path.len() as u64 + 1).to_le_bytes());
        f.extend_from_slice(&ph);
        f.extend_from_slice(path);
        f.push(0);
        f.resize(f.len().next_multiple_of(4), 0);
        let code_at = f.len() as u64;
        f[24..32].copy_from_slice(&(0x400000 + code_at).to_le_bytes());
        f.extend_from_slice(&code);
        let size = f.len() as u64;
        f[64 + 32..64 + 40].copy_from_slice(&size.to_le_bytes());
        f[64 + 40..64 + 48].copy_from_slice(&size.to_le_bytes());
        f
    }

    fn read_u64(mm: &Mm, addr: u64) -> u64 {
        let mut b = [0u8; 8];
        mm.read(UserSlice::new(addr, 8), &mut b).unwrap();
        u64::from_le_bytes(b)
    }

    fn no_interp(_: &[u8]) -> Result<Executable, Errno> {
        Err(Errno::ENOENT)
    }

    fn exe(bytes: &[u8]) -> Executable {
        Executable::from_bytes(bytes.to_vec())
    }

    /// 读出初始栈上的 auxv。
    fn auxv(mm: &Mm, sp: u64) -> BTreeMap<u64, u64> {
        let argc = read_u64(mm, sp);
        let mut p = sp + 8 * (argc + 2);
        while read_u64(mm, p) != 0 {
            p += 8;
        }
        p += 8;
        let mut out = BTreeMap::new();
        loop {
            let (k, v) = (read_u64(mm, p), read_u64(mm, p + 8));
            if k == AT_NULL {
                return out;
            }
            out.insert(k, v);
            p += 16;
        }
    }

    #[test]
    fn loads_segment_and_builds_stack() {
        let mut mm = Mm::new();
        let code = 0xd503201fu32.to_le_bytes(); // nop
        let argv = vec![b"prog".to_vec(), b"arg1".to_vec()];
        let envp = vec![b"K=V".to_vec()];
        let args = Args {
            argv: &argv,
            envp: &envp,
            execfn: b"/bin/prog",
            ids: [0; 4],
        };
        let image = load(
            &mut mm,
            &exe(&tiny_elf(ET_EXEC, &code)),
            &args,
            &mut no_interp,
        )
        .unwrap();
        assert_eq!(image.entry, 0x400078);
        assert_eq!(image.sp % 16, 0);
        assert_eq!(read_u64(&mm, 0x400078) as u32, 0xd503201f);
        assert!(mm.mmu().translate(0x400000, prot::EXEC).is_some());
        assert!(mm.mmu().translate(0x400000, prot::WRITE).is_none());

        let sp = image.sp;
        assert_eq!(read_u64(&mm, sp), 2);
        assert_eq!(mm.read_cstr(read_u64(&mm, sp + 8), 64).unwrap(), b"prog");
        assert_eq!(mm.read_cstr(read_u64(&mm, sp + 16), 64).unwrap(), b"arg1");
        assert_eq!(read_u64(&mm, sp + 24), 0);
        assert_eq!(mm.read_cstr(read_u64(&mm, sp + 32), 64).unwrap(), b"K=V");
        assert_eq!(read_u64(&mm, sp + 40), 0);

        let aux = auxv(&mm, sp);
        assert_eq!(aux[&AT_PAGESZ], PAGE_SIZE);
        assert_eq!(aux[&AT_ENTRY], 0x400078);
        assert_eq!(aux[&AT_HWCAP], appine_engine::guest_hwcap());
        assert_eq!(aux[&AT_PHDR], 0x400040);
        assert_eq!((aux[&AT_PHENT], aux[&AT_PHNUM]), (56, 1));
        assert_eq!(aux[&AT_BASE], 0);
        assert_eq!(mm.read_cstr(aux[&AT_EXECFN], 64).unwrap(), b"/bin/prog");
        assert_eq!(mm.read_cstr(aux[&AT_PLATFORM], 64).unwrap(), b"aarch64");
        let mut random = [0u8; 16];
        mm.read(UserSlice::new(aux[&AT_RANDOM], 16), &mut random)
            .unwrap();
        assert_ne!(random, [0; 16]);
        assert_eq!(aux[&AT_MINSIGSTKSZ], crate::signal::MIN_SIGSTKSZ);
        // /proc/<pid>/cmdline 和 environ 读的范围
        assert_eq!(mm.args.1 - mm.args.0, b"prog\0arg1\0".len() as u64);
        assert_eq!(mm.read_cstr(mm.args.0, 64).unwrap(), b"prog");
        assert_eq!((mm.env.0, mm.env.1 - mm.env.0), (mm.args.1, 4));
        // vDSO 只读可执行，开头是 ELF 头
        let vdso = aux[&AT_SYSINFO_EHDR];
        assert_eq!(vdso, mm.vdso);
        assert!(mm.mmu().translate(vdso, prot::EXEC).is_some());
        assert!(mm.mmu().translate(vdso, prot::WRITE).is_none());
        let mut magic = [0u8; 4];
        mm.read(UserSlice::new(vdso, 4), &mut magic).unwrap();
        assert_eq!(&magic, b"\x7fELF");
    }

    #[test]
    fn loads_pie_with_interpreter() {
        let code = 0xd503201fu32.to_le_bytes();
        let main = with_interp(tiny_elf(ET_DYN, &code), b"/lib/ld.so");
        let mut ld = tiny_elf(ET_DYN, &code);
        ld[24..32].copy_from_slice(&0x400078u64.to_le_bytes());
        let mut requested = Vec::new();
        let mut read_interp = |path: &[u8]| {
            requested = path.to_vec();
            Ok(exe(&ld))
        };
        let mut mm = Mm::new();
        let args = Args {
            argv: &[b"prog".to_vec()],
            envp: &[],
            execfn: b"prog",
            ids: [0; 4],
        };
        let image = load(&mut mm, &exe(&main), &args, &mut read_interp).unwrap();
        assert_eq!(requested, b"/lib/ld.so");

        let aux = auxv(&mm, image.sp);
        let exe_base = ET_DYN_BASE - 0x400000;
        assert_eq!(aux[&AT_PHDR], exe_base + 0x400040);
        assert_eq!(aux[&AT_PHNUM], 2);
        assert_eq!(aux[&AT_ENTRY], exe_base + u64_at(&main, 24).unwrap());
        let interp_base = aux[&AT_BASE];
        assert!(interp_base > ET_DYN_BASE && interp_base.is_multiple_of(PAGE_SIZE));
        assert_eq!(image.entry, interp_base + 0x78);
        assert!(mm.mmu().translate(image.entry, prot::EXEC).is_some());
        assert!(mm.mmu().translate(aux[&AT_ENTRY], prot::EXEC).is_some());
    }

    #[test]
    fn bss_only_segment_may_point_past_the_end_of_file() {
        // 两个程序头：代码段，以及 filesz = 0、p_offset 越过文件末尾的 .bss 段
        let mut f = tiny_elf(ET_EXEC, &[]);
        f.truncate(64 + 56);
        f[56..58].copy_from_slice(&2u16.to_le_bytes());
        let code_at = 64 + 2 * 56;
        f[24..32].copy_from_slice(&(0x400000 + code_at as u64).to_le_bytes());
        let size = (code_at + 4) as u64;
        f[64 + 32..64 + 40].copy_from_slice(&size.to_le_bytes());
        f[64 + 40..64 + 48].copy_from_slice(&size.to_le_bytes());
        let mut bss = vec![0u8; 56];
        bss[..4].copy_from_slice(&PT_LOAD.to_le_bytes());
        bss[4..8].copy_from_slice(&(PF_R | PF_W).to_le_bytes());
        bss[8..16].copy_from_slice(&0xffe8u64.to_le_bytes());
        bss[16..24].copy_from_slice(&0x41ffe8u64.to_le_bytes());
        bss[40..48].copy_from_slice(&0x4000u64.to_le_bytes());
        f.extend_from_slice(&bss);
        f.extend_from_slice(&0xd503201fu32.to_le_bytes());

        // 内存里的内容拷贝进匿名内存；宿主文件直接映射
        let path = std::env::temp_dir().join(format!("appine-loader-{}", std::process::id()));
        std::fs::write(&path, &f).unwrap();
        let from_file = Executable::open(File::open(&path).unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        for file in [exe(&f), from_file] {
            let mut mm = Mm::new();
            let args = Args {
                argv: &[],
                envp: &[],
                execfn: b"",
                ids: [0; 4],
            };
            let image = load(&mut mm, &file, &args, &mut no_interp).unwrap();
            assert!(mm.mmu().translate(0x41ffe8, prot::WRITE).is_some());
            assert!(mm.mmu().translate(0x423fe0, prot::WRITE).is_some());
            assert_eq!(read_u64(&mm, 0x420000), 0);
            let mut insn = [0u8; 4];
            mm.read(UserSlice::new(image.entry, 4), &mut insn).unwrap();
            assert_eq!(insn, 0xd503201fu32.to_le_bytes());
            // program break 紧跟在最后一个段之后
            assert_eq!(mm.brk(0), 0x424000);
        }
    }

    #[test]
    fn rejects_unsupported_files() {
        let mut mm = Mm::new();
        let args = Args {
            argv: &[],
            envp: &[],
            execfn: b"",
            ids: [0; 4],
        };
        assert_eq!(
            load(&mut mm, &exe(b"#!/bin/sh\n"), &args, &mut no_interp),
            Err(LoadError::NotElf)
        );
        let missing = with_interp(tiny_elf(ET_EXEC, &[0; 4]), b"/lib/missing.so");
        assert_eq!(
            load(&mut Mm::new(), &exe(&missing), &args, &mut no_interp),
            Err(LoadError::Interpreter(
                b"/lib/missing.so".to_vec(),
                Errno::ENOENT
            ))
        );
    }
}
