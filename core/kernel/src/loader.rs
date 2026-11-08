//! The ELF loader (the kernel/process map).
//!
//! Supports statically linked (`ET_EXEC`) and position-independent (`ET_DYN`) aarch64 programs.
//! For a program with `PT_INTERP` the interpreter (`ld-musl` on Alpine) is loaded as well, and
//! execution starts at the interpreter's entry, which does the dynamic linking. Programs in the
//! store map their segments as private file mappings, so code pages are shared between processes
//! through the host page cache (the kernel/mm map); programs without a host file, and programs
//! whose segments share pages, copy the contents into anonymous memory. The initial stack is built
//! by Linux's conventions, with auxv entries in the same set and order as arm64 Linux gives; the
//! vDSO is mapped below the interpreter.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use appine_engine::{DbtRegs, PAGE_SIZE, VA_BITS, prot};

use crate::errno::Errno;
use crate::mm::{Mm, flags};

const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;
const PT_PHDR: u32 = 6;
/// The type of the build ID note written by Go's linker (`.note.go.buildid`), with name "Go".
const NT_GO_BUILDID: u32 = 4;
/// The most bytes of `PT_NOTE` read when looking for the Go note (Go programs have only about a
/// hundred).
const MAX_NOTES: u64 = 4096;
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

/// The initial stack top (exclusive), that is the upper bound of the 47-bit address space.
pub const STACK_TOP: u64 = 1 << 47;
/// The default stack size limit (the default soft limit of `RLIMIT_STACK`, Linux's `_STK_LIM`),
/// which is also the most host memory reserved below the stack at load time.
pub const STACK_SIZE: u64 = 8 << 20;
/// How much more the initial stack maps below the arguments (`stack_expand` of Linux's
/// `setup_arg_pages`).
const STACK_EXPAND: u64 = 128 << 10;
/// However small the stack size limit, the arguments may take at least this much (Linux's
/// `ARG_MAX`, 32 pages).
const ARG_MAX: u64 = 32 * PAGE_SIZE;
/// Load base of a position-independent main program: 2/3 of the address space, chosen the same
/// way as arm64 Linux's `ELF_ET_DYN_BASE`.
const ET_DYN_BASE: u64 = 0x5555_5555_0000;
/// The maximum length of the interpreter path.
const MAX_INTERP_PATH: u64 = 4096;
/// `sysconf(_SC_CLK_TCK)`.
pub const CLOCK_TICKS: u64 = 100;

/// Why loading failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// Not an ELF file.
    NotElf,
    /// A valid ELF, but not supported yet.
    Unsupported(&'static str),
    /// The ELF structure is corrupt.
    Malformed(&'static str),
    /// Too many arguments and environment variables.
    ArgumentsTooLong,
    /// Creating a mapping failed.
    Memory(Errno),
    /// Reading the file failed.
    Read(Errno),
    /// Reading the interpreter failed.
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
            LoadError::Read(errno) => write!(f, "cannot read the file: {errno}"),
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

/// Start-up arguments of a guest program.
#[derive(Debug, Clone, Copy)]
pub struct Args<'a> {
    /// argv.
    pub argv: &'a [Vec<u8>],
    /// envp.
    pub envp: &'a [Vec<u8>],
    /// The program path, put into `AT_EXECFN`.
    pub execfn: &'a [u8],
    /// The new program's real and effective user IDs and group IDs (`AT_UID` and so on).
    pub ids: [u32; 4],
    /// A privileged exec (setuid, setgid, file capabilities, [`crate::cred::Cred::exec`]): sets
    /// `AT_SECURE`.
    pub secure: bool,
    /// The stack size limit (the soft limit of `RLIMIT_STACK`): decides the address-space layout,
    /// how much the arguments may take, and how large the initial stack mapping is.
    pub stack_limit: u64,
}

/// The load result: pc and sp when the guest starts running, and the address ranges of the program
/// and the interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// The entry address (the interpreter's entry when there is one).
    pub entry: u64,
    /// The initial stack pointer, pointing at argc.
    pub sp: u64,
    /// `[start, end)` of the main program's mapping.
    pub main: (u64, u64),
    /// The interpreter's path and `[start, end)` of its mapping.
    pub interp: Option<(Vec<u8>, u64, u64)>,
}

struct Segment {
    vaddr: u64,
    memsz: u64,
    offset: usize,
    filesz: usize,
    prot: u32,
}

/// An executable file to load: its beginning (or its whole contents), and the host file that can
/// be mapped straight into the guest address space.
pub struct Executable {
    /// The first [`Executable::HEAD`] bytes of the file: the ELF header, program headers,
    /// interpreter path and `#!` line are usually in them. The whole contents when there is no host
    /// file. Earlier the whole file was mapped in and then parsed, costing each execve one more
    /// mapping, page fault and unmapping each for the program and the interpreter.
    head: Arc<[u8]>,
    /// The size of the file.
    len: usize,
    file: Option<Arc<File>>,
}

impl Executable {
    /// How much of the host file's beginning is read in.
    const HEAD: usize = 4096;

    /// A host file (an object in the store).
    pub fn open(file: File) -> Result<Executable, Errno> {
        use std::os::unix::fs::FileExt;
        let io = |e: std::io::Error| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0));
        let len = file.metadata().map_err(io)?.len() as usize;
        let mut head = vec![0u8; len.min(Self::HEAD)];
        file.read_exact_at(&mut head, 0).map_err(io)?;
        Ok(Executable {
            head: head.into(),
            len,
            file: Some(Arc::new(file)),
        })
    }

    /// Another handle to the same file ([`ExeCache`]).
    fn share(&self) -> Executable {
        Executable {
            head: self.head.clone(),
            len: self.len,
            file: self.file.clone(),
        }
    }

    /// Contents in memory, with no host file: the segments are copied into anonymous memory.
    #[cfg(test)]
    pub fn from_bytes(data: Vec<u8>) -> Executable {
        Executable {
            len: data.len(),
            head: data.into(),
            file: None,
        }
    }

    /// The beginning of the file (at least [`Executable::HEAD`] bytes, or the whole contents if the
    /// file is shorter).
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// The contents `[off, off + n)` of the file; None if it goes past the end of the file.
    fn read(&self, off: u64, n: u64) -> Result<Option<std::borrow::Cow<'_, [u8]>>, Errno> {
        use std::os::unix::fs::FileExt;
        let Some(end) = off.checked_add(n).filter(|&e| e <= self.len as u64) else {
            return Ok(None);
        };
        let (off, end) = (off as usize, end as usize);
        if end <= self.head.len() {
            return Ok(Some(self.head[off..end].into()));
        }
        let file = self.file.as_ref().ok_or(Errno::EIO)?;
        let mut buf = vec![0u8; end - off];
        file.read_exact_at(&mut buf, off as u64)
            .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))?;
        Ok(Some(buf.into()))
    }

    /// The host file (which `/proc/<pid>/exe` records, [`crate::process::ExeFile`]).
    pub fn host(&self) -> Option<Arc<File>> {
        self.file.clone()
    }
}

/// Recently executed program files: the open host file and the beginning read in. Executing the
/// same version ([`crate::mm::content_version`]) again uses them directly, without opening,
/// reading and closing again (each process a shell starts does that once each for the program
/// and the interpreter, all with the BKL held).
#[derive(Default)]
pub struct ExeCache(Mutex<Vec<CachedExe>>);

struct CachedExe {
    /// The inode in the store.
    ino: u64,
    exe: Executable,
    /// The host file's (device number, inode) and the content version when read in.
    version: (u64, u64, u64, u64),
    /// The sequence number of the latest use; when full, the least recently used is removed.
    used: u64,
}

impl ExeCache {
    /// The most programs to remember.
    const MAX: usize = 32;

    /// Opens inode `ino` in the store (`open` opens its host file) as a file to execute.
    pub fn open(
        &self,
        ino: u64,
        open: impl FnOnce() -> Result<File, Errno>,
    ) -> Result<Executable, Errno> {
        let mut cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let next = cache.iter().map(|c| c.used).max().unwrap_or(0) + 1;
        if let Some(i) = cache.iter().position(|c| c.ino == ino) {
            let c = &mut cache[i];
            let fd = c.exe.file.as_ref().map(|f| f.as_raw_fd());
            if fd.and_then(Self::version) == Some(c.version) {
                c.used = next;
                return Ok(c.exe.share());
            }
            cache.swap_remove(i);
        }
        drop(cache);
        let exe = Executable::open(open()?)?;
        let version = exe.file.as_ref().and_then(|f| Self::version(f.as_raw_fd()));
        if let Some(version) = version {
            let mut cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if cache.len() >= Self::MAX
                && let Some(old) = (0..cache.len()).min_by_key(|&i| cache[i].used)
            {
                cache.swap_remove(old);
            }
            let used = cache.iter().map(|c| c.used).max().unwrap_or(0) + 1;
            cache.push(CachedExe {
                ino,
                exe: exe.share(),
                version,
                used,
            });
        }
        Ok(exe)
    }

    /// Inode `ino`'s data moved to another object (`syscall::fs::rehome`): what was read from the
    /// old one must not be used again, although that object did not change.
    pub fn forget(&self, ino: u64) {
        let mut cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        cache.retain(|c| c.ino != ino);
    }

    /// The identity and content version of host file `fd`; None when there is no trustworthy
    /// version.
    fn version(fd: i32) -> Option<(u64, u64, u64, u64)> {
        let st = appine_hal::fstat(fd).ok()?;
        let (dev, ino) = (st.st_dev, st.st_ino);
        crate::mm::content_version(&st, dev, ino).map(|(ctime, size)| (dev, ino, ctime, size))
    }

    /// Drops everything (when host memory is tight).
    pub fn clear(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// A parsed ELF file.
struct Elf<'a> {
    exe: &'a Executable,
    /// The host file that can be mapped directly.
    fd: Option<i32>,
    e_type: u16,
    entry: u64,
    phoff: u64,
    phnum: u64,
    phdr_vaddr: Option<u64>,
    interp: Option<Vec<u8>>,
    segments: Vec<Segment>,
    /// A program built by Go (with Go's build ID note).
    go: bool,
}

impl<'a> Elf<'a> {
    fn parse(exe: &'a Executable) -> Result<Self, LoadError> {
        let file = exe.head();
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
            exe,
            fd: exe.file.as_ref().map(AsRawFd::as_raw_fd),
            e_type,
            entry,
            phoff,
            phnum,
            phdr_vaddr: None,
            interp: None,
            segments: Vec::new(),
            go: false,
        };
        if phoff.checked_add(phnum * PHDR_SIZE).is_none() {
            return Err(LoadError::Malformed("program header out of range"));
        }
        let table = exe
            .read(phoff, phnum * PHDR_SIZE)
            .map_err(LoadError::Read)?
            .ok_or(LoadError::Malformed("truncated"))?;
        for ph in (0..phnum as usize).map(|i| i * PHDR_SIZE as usize) {
            let p_type = u32_at(&table, ph)?;
            let flags = u32_at(&table, ph + 4)?;
            let offset = u64_at(&table, ph + 8)?;
            let vaddr = u64_at(&table, ph + 16)?;
            let filesz = u64_at(&table, ph + 32)?;
            let memsz = u64_at(&table, ph + 40)?;
            match p_type {
                PT_PHDR => elf.phdr_vaddr = Some(vaddr),
                PT_INTERP => {
                    if filesz <= 1 || filesz > MAX_INTERP_PATH {
                        return Err(LoadError::Malformed("bad interpreter path"));
                    }
                    let range = exe
                        .read(offset, filesz)
                        .map_err(LoadError::Read)?
                        .ok_or(LoadError::Malformed("bad interpreter path"))?;
                    let path = range
                        .strip_suffix(&[0])
                        .ok_or(LoadError::Malformed("bad interpreter path"))?;
                    if path.contains(&0) {
                        return Err(LoadError::Malformed("bad interpreter path"));
                    }
                    elf.interp = Some(path.to_vec());
                }
                // Only selects Tier 1's register allocation (load_prepared); if it cannot be read,
                // treat the program as not Go
                PT_NOTE if !elf.go && filesz <= MAX_NOTES => {
                    elf.go =
                        matches!(exe.read(offset, filesz), Ok(Some(notes)) if has_go_note(&notes));
                }
                PT_LOAD => {
                    if filesz > memsz {
                        return Err(LoadError::Malformed(
                            "segment file size exceeds memory size",
                        ));
                    }
                    // A segment with only .bss has filesz 0 and its p_offset may point past the
                    // end of the file; Linux does not read it
                    let file_end = offset.checked_add(filesz);
                    if filesz > 0 && file_end.is_none_or(|end| end > exe.len as u64) {
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

    /// Maps the segments into `mm` with bias `bias`.
    fn map(&self, mm: &mut Mm, bias: u64) -> Result<Loaded, LoadError> {
        let (_, hi) = self.span();
        if bias
            .checked_add(hi)
            .is_none_or(|end| end > STACK_TOP - STACK_SIZE)
        {
            return Err(LoadError::Unsupported("segment overlaps the stack"));
        }
        // Whether segments share pages: after sorting by start it is enough to look at adjacent
        // pairs (busybox plus ld-musl have about four hundred pages; earlier each page went into a
        // BTreeMap, on every execve)
        let mut spans: Vec<(u64, u64)> = self
            .segments
            .iter()
            .map(|seg| {
                let start = (bias + seg.vaddr) & !(PAGE_SIZE - 1);
                (
                    start,
                    (bias + seg.vaddr + seg.memsz).next_multiple_of(PAGE_SIZE),
                )
            })
            .collect();
        spans.sort_unstable();
        let shared_page = spans.windows(2).any(|w| w[1].0 < w[0].1);
        let loaded = Loaded {
            bias,
            entry: bias + self.entry,
            phdr: bias + self.phdr_vaddr()?,
            phnum: self.phnum,
            end: bias + hi,
        };
        // When file offsets and addresses agree within the page (Linux requires this too) and no
        // segments share pages, map the host file directly
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
        // Each guest page's permissions are the union of all the segments covering it
        let mut pages: BTreeMap<u64, u32> = BTreeMap::new();
        for seg in &self.segments {
            let mut page = (bias + seg.vaddr) & !(PAGE_SIZE - 1);
            while page < bias + seg.vaddr + seg.memsz {
                *pages.entry(page).or_insert(0) |= seg.prot;
                page += PAGE_SIZE;
            }
        }
        // Adjacent pages with the same permissions are merged into one mapping
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
            let data = self
                .exe
                .read(seg.offset as u64, seg.filesz as u64)
                .map_err(LoadError::Read)?
                .ok_or(LoadError::Malformed("segment exceeds the file"))?;
            mm.write_kernel(bias + seg.vaddr, &data)
                .map_err(LoadError::Memory)?;
        }
        Ok(loaded)
    }

    /// The page range `[start, end)` the segments cover (without the bias).
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

    /// The in-memory address of the program headers (without the bias): `PT_PHDR` if present,
    /// otherwise the segment containing the program headers.
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

    /// Maps one segment as a private file mapping: the pages holding file contents map the host
    /// file, the part of the last page after the file contents is zeroed (the start of .bss), and
    /// the rest of .bss is anonymous memory.
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

/// An ELF after mapping into memory.
struct Loaded {
    bias: u64,
    entry: u64,
    phdr: u64,
    phnum: u64,
    end: u64,
}

/// The callback that opens the interpreter file by guest path.
pub type ReadInterp<'a> = dyn FnMut(&[u8]) -> Result<Executable, Errno> + 'a;

/// Loads ELF file `file` into `mm` and builds the initial stack by Linux's conventions.
///
/// When the program has `PT_INTERP`, `read_interp` reads the interpreter file by guest path.
pub fn load(
    mm: &mut Mm,
    file: &Executable,
    args: &Args<'_>,
    read_interp: &mut ReadInterp<'_>,
) -> Result<Image, LoadError> {
    load_prepared(mm, &prepare(file, read_interp)?, args)
}

/// A program and its interpreter, already opened and format-checked ([`prepare`]), needing only
/// mapping ([`load_prepared`]).
pub struct Prepared<'a> {
    exe: Elf<'a>,
    interp: Option<(Vec<u8>, Executable)>,
}

/// The first half of loading: parses `file`, and with `PT_INTERP` opens the interpreter with
/// `read_interp`. Opening the interpreter resolves a guest path, so the caller holds the BKL; the
/// second half, [`load_prepared`], touches only the new address space, and execve runs it with
/// the BKL released.
pub fn prepare<'a>(
    file: &'a Executable,
    read_interp: &mut ReadInterp<'_>,
) -> Result<Prepared<'a>, LoadError> {
    let exe = Elf::parse(file)?;
    let interp = match &exe.interp {
        Some(path) => {
            let data = read_interp(path).map_err(|e| LoadError::Interpreter(path.clone(), e))?;
            Some((path.clone(), data))
        }
        None => None,
    };
    Ok(Prepared { exe, interp })
}

/// The second half of loading: maps the program prepared by [`prepare`] into `mm` and builds the
/// initial stack.
pub fn load_prepared(
    mm: &mut Mm,
    prepared: &Prepared<'_>,
    args: &Args<'_>,
) -> Result<Image, LoadError> {
    let exe = &prepared.exe;
    mm.set_layout(args.stack_limit);
    let bias = match exe.e_type {
        ET_DYN => ET_DYN_BASE - exe.span().0,
        _ => 0,
    };
    let main = exe.map(mm, bias)?;

    let mut interp_base = 0;
    let mut interp_range = None;
    let mut entry = main.entry;
    if let Some((path, data)) = &prepared.interp {
        let interp = Elf::parse(data)?;
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
    mm.dbt_regs = dbt_regs(exe.go);

    mm.set_brk(main.end);
    let mut random = [0u8; 16];
    appine_hal::fill_random(&mut random).map_err(|e| LoadError::Memory(Errno(e)))?;
    // arm64's ARCH_DLINFO (AT_SYSINFO_EHDR, AT_MINSIGSTKSZ) comes first
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
        (AT_SECURE, args.secure as u64),
        (AT_RANDOM, 0),
        (AT_HWCAP2, appine_engine::guest_hwcap2()),
        (AT_EXECFN, 0),
        (AT_PLATFORM, 0),
    ];
    let StackLayout {
        sp,
        args,
        env,
        auxv,
    } = build_stack(mm, args, &auxv, &random)?;
    mm.args = args;
    mm.env = env;
    mm.auxv = auxv;
    Ok(Image {
        entry,
        sp,
        main: (main.bias + exe.span().0, main.end),
        interp: interp_range,
    })
}

/// Which register set Tier 1 uses for the program: Go programs use a separate one
/// ([`DbtRegs::Go`]). The environment variable `APPINE_DBT_REGS` (`go` or `default`) makes every
/// program use the same set, for testing the other one.
fn dbt_regs(go: bool) -> DbtRegs {
    static FORCED: OnceLock<Option<DbtRegs>> = OnceLock::new();
    let forced = *FORCED.get_or_init(|| match std::env::var("APPINE_DBT_REGS").as_deref() {
        Ok("go") => Some(DbtRegs::Go),
        Ok("default") => Some(DbtRegs::Default),
        _ => None,
    });
    forced.unwrap_or(if go { DbtRegs::Go } else { DbtRegs::Default })
}

/// The layout of the initial stack.
struct StackLayout {
    /// The stack pointer, pointing at argc.
    sp: u64,
    /// The range of the argv strings.
    args: (u64, u64),
    /// The range of the envp strings.
    env: (u64, u64),
    /// The auxv on the stack (`/proc/<pid>/auxv`).
    auxv: Rc<[u64]>,
}

/// Builds the initial stack in Linux's layout:
///
/// ```text
/// sp → argc | argv[0..n] | NULL | envp[0..m] | NULL | auxv pairs | AT_NULL
///      ...
///      16 bytes of AT_RANDOM | strings of argv, envp, AT_EXECFN, AT_PLATFORM
/// STACK_TOP
/// ```
///
/// The stack is first mapped [`STACK_EXPAND`] further below the arguments (within the stack size
/// limit), then grows down on demand ([`Mm::grow_down`]), the same as Linux's `setup_arg_pages`.
///
/// The values of `AT_RANDOM`, `AT_EXECFN` and `AT_PLATFORM` in `auxv` are filled in here. Returns
/// sp and the ranges of the argv and envp strings.
fn build_stack(
    mm: &mut Mm,
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
    if strings + words * 8 > arg_limit(args.stack_limit) {
        return Err(LoadError::ArgumentsTooLong);
    }

    let strings_start = (STACK_TOP - 16 - strings as u64) & !15;
    let sp = (strings_start - words as u64 * 8) & !15;
    let used = STACK_TOP - (sp & !(PAGE_SIZE - 1));
    let limit = args.stack_limit & !(PAGE_SIZE - 1);
    let len = (used + STACK_EXPAND).min(limit).max(used);
    mm.map_stack(len, limit.min(STACK_SIZE))
        .map_err(LoadError::Memory)?;
    mm.set_start_stack(sp);
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

    debug_assert_eq!(table.len(), words);
    let bytes: Vec<u8> = table.iter().flat_map(|w| w.to_le_bytes()).collect();
    mm.write_kernel(sp, &bytes).map_err(LoadError::Memory)?;
    Ok(StackLayout {
        sp,
        args: (args_start, env_start),
        env: (env_start, env_end),
        auxv: Rc::from(&table[table.len() - 2 * (auxv.len() + 1)..]),
    })
}

/// The most stack space the strings and pointers of argv and envp may take; beyond it is `E2BIG`.
/// The same as Linux's `bprm_stack_limits`: 1/4 of the stack size limit, at most 6 MB (3/4 of
/// `_STK_LIM`), at least [`ARG_MAX`].
pub fn arg_limit(stack_limit: u64) -> usize {
    (STACK_SIZE / 4 * 3).min(stack_limit / 4).max(ARG_MAX) as usize
}

/// Whether a `PT_NOTE` contains Go's build ID note.
fn has_go_note(mut notes: &[u8]) -> bool {
    while let (Ok(namesz), Ok(descsz), Ok(kind)) =
        (u32_at(notes, 0), u32_at(notes, 4), u32_at(notes, 8))
    {
        let name = notes.get(12..12 + namesz as usize);
        if kind == NT_GO_BUILDID && name.is_some_and(|n| n.starts_with(b"Go\0")) {
            return true;
        }
        let end =
            12 + (namesz as usize).next_multiple_of(4) + (descsz as usize).next_multiple_of(4);
        let Some(rest) = notes.get(end..) else {
            return false;
        };
        notes = rest;
    }
    false
}

fn to_prot(flags: u32) -> u32 {
    let mut p = 0;
    if flags & PF_R != 0 {
        p |= prot::READ;
    }
    if flags & PF_W != 0 {
        // arm64 Linux has no write-only pages: writable implies readable
        p |= prot::READ | prot::WRITE;
    }
    if flags & PF_X != 0 {
        p |= prot::EXEC;
    }
    p
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

fn bytes_at<const N: usize>(file: &[u8], off: usize) -> Result<[u8; N], LoadError> {
    file.get(
        off..off
            .checked_add(N)
            .ok_or(LoadError::Malformed("truncated"))?,
    )
    .map(|b| b.try_into().unwrap())
    .ok_or(LoadError::Malformed("truncated"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uaccess::UserSlice;

    /// Builds a minimal ELF with a single PT_LOAD segment, with code at 0x400000 + 0x78.
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

    /// Appends a PT_INTERP program header and a path after `tiny_elf`.
    fn with_interp(f: Vec<u8>, path: &[u8]) -> Vec<u8> {
        with_segment(f, PT_INTERP, &[path, &[0]].concat())
    }

    /// Appends a program header of type `p_type` after `tiny_elf`, with contents `data`.
    fn with_segment(mut f: Vec<u8>, p_type: u32, data: &[u8]) -> Vec<u8> {
        let code = f.split_off(0x78);
        f.truncate(64 + 56);
        f[56..58].copy_from_slice(&2u16.to_le_bytes());
        let data_at = 64 + 2 * 56;
        let mut ph = vec![0u8; 56];
        ph[..4].copy_from_slice(&p_type.to_le_bytes());
        ph[8..16].copy_from_slice(&(data_at as u64).to_le_bytes());
        ph[32..40].copy_from_slice(&(data.len() as u64).to_le_bytes());
        f.extend_from_slice(&ph);
        f.extend_from_slice(data);
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

    /// Reads the auxv on the initial stack.
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
    fn go_programs_get_their_own_tier1_registers() {
        let note = |name: &[u8], kind: u32, desc: &[u8]| {
            let mut n = Vec::new();
            for word in [name.len() as u32, desc.len() as u32, kind] {
                n.extend_from_slice(&word.to_le_bytes());
            }
            n.extend_from_slice(name);
            n.resize(n.len().next_multiple_of(4), 0);
            n.extend_from_slice(desc);
            n.resize(n.len().next_multiple_of(4), 0);
            n
        };
        let regs = |notes: &[u8]| {
            let mut mm = Mm::new();
            let elf = with_segment(
                tiny_elf(ET_EXEC, &0xd503201fu32.to_le_bytes()),
                PT_NOTE,
                notes,
            );
            let args = Args {
                argv: &[b"prog".to_vec()],
                envp: &[],
                execfn: b"/bin/prog",
                ids: [0; 4],
                secure: false,
                stack_limit: STACK_SIZE,
            };
            load(&mut mm, &exe(&elf), &args, &mut no_interp).unwrap();
            mm.dbt_regs
        };
        // An externally linked Go program (Alpine's go) has a GNU build ID first, then Go's
        let gnu = note(b"GNU\0", 3, &[0xab; 20]);
        let go = note(b"Go\0\0", NT_GO_BUILDID, b"abc/def");
        assert_eq!(regs(&[&gnu[..], &go].concat()), DbtRegs::Go);
        assert_eq!(regs(&go), DbtRegs::Go);
        assert_eq!(regs(&gnu), DbtRegs::Default);
        // A wrong type or a truncated name does not count
        assert_eq!(regs(&note(b"Go\0\0", 1, b"x")), DbtRegs::Default);
        assert_eq!(regs(&go[..14]), DbtRegs::Default);
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
            secure: false,
            stack_limit: STACK_SIZE,
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
        assert_eq!(aux[&AT_HWCAP2], appine_engine::guest_hwcap2());
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
        // The ranges read by /proc/<pid>/cmdline and environ
        assert_eq!(mm.args.1 - mm.args.0, b"prog\0arg1\0".len() as u64);
        assert_eq!(mm.read_cstr(mm.args.0, 64).unwrap(), b"prog");
        assert_eq!((mm.env.0, mm.env.1 - mm.env.0), (mm.args.1, 4));
        // The vDSO is read-only and executable, starting with an ELF header
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
            secure: false,
            stack_limit: STACK_SIZE,
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
        // Two program headers: the code segment, and a .bss segment with filesz = 0 and p_offset
        // past the end of the file
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

        // Contents in memory are copied into anonymous memory; a host file is mapped directly
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
                secure: false,
                stack_limit: STACK_SIZE,
            };
            let image = load(&mut mm, &file, &args, &mut no_interp).unwrap();
            assert!(mm.mmu().translate(0x41ffe8, prot::WRITE).is_some());
            assert!(mm.mmu().translate(0x423fe0, prot::WRITE).is_some());
            assert_eq!(read_u64(&mm, 0x420000), 0);
            let mut insn = [0u8; 4];
            mm.read(UserSlice::new(image.entry, 4), &mut insn).unwrap();
            assert_eq!(insn, 0xd503201fu32.to_le_bytes());
            // The program break follows right after the last segment
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
            secure: false,
            stack_limit: STACK_SIZE,
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
