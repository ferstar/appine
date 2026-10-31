//! core dump（Linux 的 `do_coredump`、`elf_core_dump`，设计文档 §7.7）。
//!
//! 默认动作是生成 core 的信号（[`dumps_core`]）结束进程时，先结束其他线程并等它们
//! 退出：线程退出时把自己的寄存器留在线程组里（[`ThreadCore`]，`task::exit`）。然后按
//! `/proc/sys/kernel/core_pattern` 在进程的当前目录里写一个 ELF core 文件：每个映射一个
//! `PT_LOAD`（写多少见 [`Mm::dump_regions`]），一个 `PT_NOTE` 里是每个线程的寄存器
//! （`NT_PRSTATUS`、`NT_PRFPREG`、`NT_ARM_TLS` 等）和进程的 `NT_PRPSINFO`、
//! `NT_SIGINFO`、`NT_AUXV`、`NT_FILE`，与 arm64 Linux 写的相同，gdb 可以事后分析。
//!
//! 与 Linux 一样，`RLIMIT_CORE` 小于一页、进程不可 dump（`PR_SET_DUMPABLE`、setuid
//! 程序）时不写；写出的字节（空洞不算）到了 `RLIMIT_CORE` 就截断。管道形式的
//! `core_pattern`（`|程序`）不支持，不写。建成了 core 文件的进程（截断了也算，与 Linux
//! 的 `has_dumped` 一样），wait 的状态带 `WCOREDUMP`，SIGCHLD、waitid 的 `si_code` 是
//! `CLD_DUMPED`。

use crate::errno::Errno;
use crate::fd::FileRef;
use crate::kernel::{Cx, Tid, wake};
use crate::mm::{DumpRegion, Mm};
use crate::process::{CpuTime, RLIMIT_CORE, Signal, ThreadGroup};
use crate::ptrace::{HwDebug, Regs};
use crate::signal::SigInfo;
use crate::task::Task;
use appine_engine::{CpuState, PAGE_SIZE, prot};

/// `core_pattern` 的最大长度（Linux 的 `CORENAME_MAX_SIZE`）。
pub(crate) const PATTERN_MAX: usize = 128;

/// 默认动作是生成 core 的信号（Linux 的 `SIG_KERNEL_COREDUMP_MASK`）：SIGQUIT、
/// SIGILL、SIGTRAP、SIGABRT、SIGBUS、SIGFPE、SIGSEGV、SIGXCPU、SIGXFSZ、SIGSYS。
pub(crate) fn dumps_core(sig: Signal) -> bool {
    matches!(sig.number(), 3..=8 | 11 | 24 | 25 | 31)
}

/// 一个线程留给 core 文件的状态（Linux 的 `elf_thread_core_info`）。
pub(crate) struct ThreadCore {
    tid: Tid,
    regs: Regs,
    /// 跟踪者设置的硬件断点和观察点（`NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH`）。
    hw: HwDebug,
    /// 线程自己挂起的信号和屏蔽字。
    pending: u64,
    blocked: u64,
    cpu: CpuTime,
}

impl ThreadCore {
    /// 线程 `task` 现在的状态，`state` 是它的寄存器。
    pub(crate) fn of(task: &Task, state: &CpuState) -> ThreadCore {
        ThreadCore {
            tid: task.tid.get(),
            regs: Regs::save(state),
            hw: task.trace.borrow().hw,
            pending: task.pending.borrow().set(),
            blocked: task.sigmask.get(),
            cpu: task.cpu_self(),
        }
    }
}

/// 致命信号 `sig`（附带 `info`）正在结束调用者的线程组：写 core 文件。调用方已经让
/// 线程组退出（`signal::group_exit`），这里等其他线程退出、留下寄存器，再写。返回是否
/// 写成了。
pub(crate) fn dump(cx: &mut Cx<'_>, state: &CpuState, sig: Signal, info: &SigInfo) -> bool {
    let group = cx.t.group.clone();
    {
        let g = group.borrow();
        // 与 Linux 一样，限制小于 ELF 的最小 core（一页）时不写
        if g.rlimits[RLIMIT_CORE as usize].0 < PAGE_SIZE || !g.dumpable {
            return false;
        }
    }
    let k = cx.kr();
    if k.core_pattern.first() == Some(&b'|') {
        crate::debug::log(format_args!(
            "core dump: pipe core_pattern is not supported"
        ));
        return false;
    }
    // 等其他线程退出，它们退出时把寄存器留在 group.core 里
    group.borrow_mut().core = Some(Vec::new());
    while group.borrow().threads.len() > 1 {
        let _ = cx.wait_on(None, &mut [], wake::EXITED);
    }
    let others = group.borrow_mut().core.take().unwrap_or_default();
    let mut threads = vec![ThreadCore::of(cx.task, state)];
    threads.extend(others);
    let name = core_name(cx, sig);
    match write(cx, &name, &threads, sig, info) {
        Ok(()) => true,
        Err(e) => {
            crate::debug::log(format_args!(
                "core dump: {}: errno {}",
                String::from_utf8_lossy(&name),
                e.0
            ));
            false
        }
    }
}

/// 按 `core_pattern` 展开 core 文件的名字（Linux 的 `format_corename`）。
fn core_name(cx: &Cx<'_>, sig: Signal) -> Vec<u8> {
    let k = cx.kr();
    let g = cx.t.group.borrow();
    let pattern = k.core_pattern.clone();
    let mut out = Vec::new();
    let mut has_pid = false;
    let mut it = pattern.iter().copied();
    while let Some(c) = it.next() {
        if c != b'%' {
            out.push(c);
            continue;
        }
        let spec = match it.next() {
            Some(s) => s,
            None => break,
        };
        // %p、%i 是进程自己的 pid 命名空间里的编号，%P、%I 是开机时的命名空间里的
        let vnr = |id: Tid| g.pid_ns.vnr(id).unwrap_or(0).to_string();
        let text = match spec {
            b'%' => "%".to_owned(),
            b'p' => {
                has_pid = true;
                vnr(g.tgid)
            }
            b'P' => {
                has_pid = true;
                g.tgid.to_string()
            }
            b'i' => vnr(cx.tid()),
            b'I' => cx.tid().to_string(),
            b'u' => g.cred.ruid.to_string(),
            b'g' => g.cred.rgid.to_string(),
            b's' => sig.number().to_string(),
            b't' => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .to_string(),
            b'h' => {
                String::from_utf8_lossy(&cx.task.ns.borrow().uts.hostname.borrow()).into_owned()
            }
            b'e' => {
                // 名字里的 / 换成 !（Linux 的 cn_esc_printf）
                let comm = cx.task.comm.get();
                let len = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
                String::from_utf8_lossy(&comm[..len]).replace('/', "!")
            }
            b'E' => String::from_utf8_lossy(&g.exe).replace('/', "!"),
            b'c' => g.rlimits[RLIMIT_CORE as usize].0.to_string(),
            b'd' => (!g.dumpable as u32).to_string(),
            // 不认识的说明符：与 Linux 一样什么也不输出
            _ => String::new(),
        };
        out.extend_from_slice(text.as_bytes());
    }
    if k.core_uses_pid && !has_pid {
        let pid = g.pid_ns.vnr(g.tgid).unwrap_or(0);
        out.extend_from_slice(format!(".{pid}").as_bytes());
    }
    out
}

/// 在进程的当前目录（名字是绝对路径时按它）写 core 文件。与 Linux 一样先删掉同名的
/// 文件，再以 0600 新建（`O_EXCL | O_NOFOLLOW`）；建成了就算写了，写到 `RLIMIT_CORE`
/// 或者写出错时截断。
fn write(
    cx: &mut Cx<'_>,
    name: &[u8],
    threads: &[ThreadCore],
    sig: Signal,
    info: &SigInfo,
) -> Result<(), Errno> {
    let mm = cx.t.mm.clone();
    let (regions, notes) = {
        let mm = mm.borrow();
        let filter = cx.t.group.borrow().coredump_filter;
        let regions = mm.dump_regions(filter);
        let notes = notes(cx, &mm, &regions, threads, sig, info);
        (regions, notes)
    };
    let limit = cx.t.group.borrow().rlimits[RLIMIT_CORE as usize].0;
    let (header, data_offset) = elf_header(&regions, &notes);
    let _ = crate::syscall::unlink_path(cx, name);
    let file = crate::syscall::create_exclusive(cx, name, 0o600)?;
    let mut w = CoreWriter {
        file,
        limit,
        written: 0,
        end: 0,
        stopped: false,
    };
    w.put(cx, 0, &header);
    let mut offset = data_offset;
    let mut page = vec![0u8; PAGE_SIZE as usize];
    for r in regions.iter().filter(|r| r.size > 0) {
        // 匿名内存、共享内存里没碰过的页留空洞（Linux 的 FOLL_DUMP）
        let resident = if r.sparse {
            mm.borrow().residency(r.start, r.size).ok()
        } else {
            None
        };
        let mut addr = r.start;
        while addr < r.start + r.size && !w.stopped {
            let index = ((addr - r.start) / PAGE_SIZE) as usize;
            let touched = resident
                .as_ref()
                .is_none_or(|v| v.get(index) == Some(&true));
            if touched && mm.borrow().dump_page(addr, &mut page) {
                w.put(cx, offset, &page);
            }
            addr += PAGE_SIZE;
            offset += PAGE_SIZE;
        }
    }
    // 最后是空洞时文件也要有完整的长度
    if w.end < offset {
        w.put(cx, offset - 1, &[0]);
    }
    Ok(())
}

/// 往 core 文件写。与 Linux 的 `dump_emit` 一样，写出的字节（空洞不算）超过
/// `RLIMIT_CORE`、或者写出错之后不再写。
struct CoreWriter {
    file: FileRef,
    limit: u64,
    written: u64,
    /// 写到的最远位置。
    end: u64,
    stopped: bool,
}

impl CoreWriter {
    fn put(&mut self, cx: &mut Cx<'_>, offset: u64, data: &[u8]) {
        if self.stopped || self.written + data.len() as u64 > self.limit {
            self.stopped = true;
            return;
        }
        let mut done = 0;
        while done < data.len() {
            match crate::syscall::write_at(cx, &self.file, &data[done..], offset + done as u64) {
                Ok(n) if n > 0 => done += n,
                _ => {
                    self.stopped = true;
                    return;
                }
            }
        }
        self.written += data.len() as u64;
        self.end = self.end.max(offset + data.len() as u64);
    }
}

// ---- ELF ----

const ET_CORE: u16 = 4;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const PN_XNUM: u16 = 0xffff;
const EHDR_SIZE: u64 = 64;
const PHDR_SIZE: u64 = 56;

const NT_PRSTATUS: u32 = 1;
const NT_PRFPREG: u32 = 2;
const NT_PRPSINFO: u32 = 3;
const NT_AUXV: u32 = 6;
const NT_SIGINFO: u32 = 0x5349_4749;
const NT_FILE: u32 = 0x4649_4c45;
const NT_ARM_TLS: u32 = 0x401;
const NT_ARM_HW_BREAK: u32 = 0x402;
const NT_ARM_HW_WATCH: u32 = 0x403;
const NT_ARM_SYSTEM_CALL: u32 = 0x404;

/// ELF 头和程序头，以及第一个映射的内容在文件里的位置（按页对齐，Linux 的 `dataoff`）。
fn elf_header(regions: &[DumpRegion], notes: &[u8]) -> (Vec<u8>, u64) {
    let segs = regions.len() + 1;
    let phnum = if segs >= PN_XNUM as usize {
        PN_XNUM
    } else {
        segs as u16
    };
    // 程序头超过 65535 个时，真正的个数放在第 0 个节头的 sh_info 里（Linux 的 PN_XNUM）
    let extnum = phnum == PN_XNUM;
    let notes_at = EHDR_SIZE + PHDR_SIZE * segs as u64;
    let shdr_at = notes_at + notes.len() as u64;
    let end = shdr_at + if extnum { 64 } else { 0 };
    let data_offset = end.next_multiple_of(PAGE_SIZE);

    let mut b = Vec::with_capacity(end as usize);
    b.extend_from_slice(b"\x7fELF\x02\x01\x01");
    b.resize(16, 0);
    b.extend_from_slice(&ET_CORE.to_le_bytes());
    b.extend_from_slice(&EM_AARCH64.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes()); // e_version
    b.extend_from_slice(&0u64.to_le_bytes()); // e_entry
    b.extend_from_slice(&EHDR_SIZE.to_le_bytes()); // e_phoff
    b.extend_from_slice(&(if extnum { shdr_at } else { 0 }).to_le_bytes()); // e_shoff
    b.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    b.extend_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    b.extend_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    b.extend_from_slice(&phnum.to_le_bytes());
    b.extend_from_slice(&(if extnum { 64u16 } else { 0 }).to_le_bytes()); // e_shentsize
    b.extend_from_slice(&(extnum as u16).to_le_bytes()); // e_shnum
    b.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx

    let phdr = |b: &mut Vec<u8>,
                ty: u32,
                flags: u32,
                off: u64,
                vaddr: u64,
                filesz: u64,
                memsz: u64,
                align: u64| {
        b.extend_from_slice(&ty.to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
        for v in [off, vaddr, 0, filesz, memsz, align] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    };
    phdr(&mut b, PT_NOTE, 0, notes_at, 0, notes.len() as u64, 0, 4);
    let mut off = data_offset;
    for r in regions {
        let mut flags = 0;
        if r.prot & prot::READ != 0 {
            flags |= 4;
        }
        if r.prot & prot::WRITE != 0 {
            flags |= 2;
        }
        if r.prot & prot::EXEC != 0 {
            flags |= 1;
        }
        phdr(
            &mut b,
            PT_LOAD,
            flags,
            off,
            r.start,
            r.size,
            r.end - r.start,
            PAGE_SIZE,
        );
        off += r.size;
    }
    b.extend_from_slice(notes);
    if extnum {
        // 节头：只有 sh_info 有用（真正的程序头个数）
        let mut sh = [0u8; 64];
        sh[44..48].copy_from_slice(&(segs as u32).to_le_bytes());
        b.extend_from_slice(&sh);
    }
    (b, data_offset)
}

/// 一个 note：名字和内容各按 4 字节对齐。
fn note(out: &mut Vec<u8>, name: &str, ty: u32, desc: &[u8]) {
    out.extend_from_slice(&(name.len() as u32 + 1).to_le_bytes());
    out.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend_from_slice(desc);
    out.resize(out.len().next_multiple_of(4), 0);
}

/// 所有 note，顺序与 Linux 的 `write_note_info` 相同：每个线程先是 `NT_PRSTATUS`，第一个
/// 线程（产生信号的那个）之后接着进程的 `NT_PRPSINFO`、`NT_SIGINFO`、`NT_AUXV`、
/// `NT_FILE`，然后是这个线程别的寄存器组。
fn notes(
    cx: &Cx<'_>,
    mm: &Mm,
    regions: &[DumpRegion],
    threads: &[ThreadCore],
    sig: Signal,
    info: &SigInfo,
) -> Vec<u8> {
    let g = cx.t.group.borrow();
    // 进程的 CPU 时间：退出了的线程已经加进 g.cpu，产生信号的线程还没有
    let process_cpu = g.cpu + threads[0].cpu;
    let mut out = Vec::new();
    for (i, t) in threads.iter().enumerate() {
        note(
            &mut out,
            "CORE",
            NT_PRSTATUS,
            &prstatus(&g, t, sig, process_cpu),
        );
        if i == 0 {
            note(&mut out, "CORE", NT_PRPSINFO, &prpsinfo(cx, &g, mm));
            note(&mut out, "CORE", NT_SIGINFO, &info.encode(sig));
            let auxv: Vec<u8> = mm.auxv.iter().flat_map(|w| w.to_le_bytes()).collect();
            note(&mut out, "CORE", NT_AUXV, &auxv);
            note(&mut out, "CORE", NT_FILE, &files(regions));
        }
        // 别的寄存器组与 PTRACE_GETREGSET 读到的相同；不在系统调用里，编号是 -1
        for (name, nt) in [
            ("CORE", NT_PRFPREG),
            ("LINUX", NT_ARM_TLS),
            ("LINUX", NT_ARM_HW_BREAK),
            ("LINUX", NT_ARM_HW_WATCH),
            ("LINUX", NT_ARM_SYSTEM_CALL),
        ] {
            let desc = crate::syscall::encode_regset(u64::from(nt), &t.regs, -1, &t.hw);
            note(&mut out, name, nt, &desc);
        }
    }
    out
}

fn timeval(ns: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
    b[8..].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
    b
}

/// `struct elf_prstatus`（arm64，392 字节）。与 Linux 一样，线程组的主线程报告整个进程
/// 的 CPU 时间（`process_cpu`），别的线程报告自己的。
fn prstatus(g: &ThreadGroup, t: &ThreadCore, sig: Signal, process_cpu: CpuTime) -> Vec<u8> {
    let mut b = vec![0u8; 392];
    b[..4].copy_from_slice(&sig.number().to_le_bytes());
    b[12..14].copy_from_slice(&(sig.number() as u16).to_le_bytes());
    b[16..24].copy_from_slice(&t.pending.to_le_bytes());
    b[24..32].copy_from_slice(&t.blocked.to_le_bytes());
    // pid 是进程自己的 pid 命名空间里的编号（Linux 的 fill_prstatus 用 task_pid_vnr）
    for (i, &v) in [t.tid, g.ppid, g.pgid, g.sid].iter().enumerate() {
        let v = g.pid_ns.vnr(v).unwrap_or(0);
        b[32 + i * 4..36 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    let cpu = if t.tid == g.tgid { process_cpu } else { t.cpu };
    b[48..64].copy_from_slice(&timeval(cpu.user_ns()));
    b[64..80].copy_from_slice(&timeval(cpu.sys_ns));
    b[80..96].copy_from_slice(&timeval(g.child_cpu.user_ns()));
    b[96..112].copy_from_slice(&timeval(g.child_cpu.sys_ns));
    let r = &t.regs;
    // user_pt_regs：x0–x30、sp、pc、pstate
    for (i, v) in r.x.iter().chain(&[r.sp, r.pc, r.nzcv]).enumerate() {
        b[112 + i * 8..120 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    b[384..388].copy_from_slice(&1i32.to_le_bytes()); // pr_fpvalid
    b
}

/// `struct elf_prpsinfo`（arm64，136 字节）：状态、凭据、pid、comm 和命令行的前 79 个
/// 字节（参数之间的 NUL 换成空格）。
fn prpsinfo(cx: &Cx<'_>, g: &ThreadGroup, mm: &Mm) -> Vec<u8> {
    const PF_FORKNOEXEC: u64 = 0x40;
    const PF_DUMPCORE: u64 = 0x200;
    const PF_SIGNALED: u64 = 0x400;
    let mut b = vec![0u8; 136];
    b[1] = b'R';
    b[3] = cx.task.nice.get() as i8 as u8;
    let flags = PF_DUMPCORE | PF_SIGNALED | if g.execed { 0 } else { PF_FORKNOEXEC };
    b[8..16].copy_from_slice(&flags.to_le_bytes());
    // 真实用户 ID、组 ID 在进程自己的用户命名空间里的值（Linux 的 fill_psinfo）
    b[16..20].copy_from_slice(&g.cred.uid_out(g.cred.ruid).to_le_bytes());
    b[20..24].copy_from_slice(&g.cred.gid_out(g.cred.rgid).to_le_bytes());
    for (i, &v) in [g.tgid, g.ppid, g.pgid, g.sid].iter().enumerate() {
        let v = g.pid_ns.vnr(v).unwrap_or(0);
        b[24 + i * 4..28 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    b[40..56].copy_from_slice(&cx.task.comm.get());
    let (start, end) = mm.args;
    let len = end.saturating_sub(start).min(79) as usize;
    let mut args = vec![0u8; len];
    if mm
        .read(crate::uaccess::UserSlice::new(start, len), &mut args)
        .is_ok()
    {
        let used = args.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
        for (i, &c) in args[..used].iter().enumerate() {
            b[56 + i] = if c == 0 { b' ' } else { c };
        }
    }
    b
}

/// `NT_FILE`：文件映射的个数、页大小、每个的 (起点, 终点, 以页计的文件偏移)，然后是
/// 各自的路径。
fn files(regions: &[DumpRegion]) -> Vec<u8> {
    let mapped: Vec<_> = regions
        .iter()
        .filter_map(|r| r.file.as_ref().map(|f| (r, f)))
        .collect();
    let mut b = Vec::new();
    b.extend_from_slice(&(mapped.len() as u64).to_le_bytes());
    b.extend_from_slice(&PAGE_SIZE.to_le_bytes());
    for (r, (offset, _)) in &mapped {
        for v in [r.start, r.end, offset / PAGE_SIZE] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    for (_, (_, path)) in &mapped {
        b.extend_from_slice(path);
        b.push(0);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_padded_to_four_bytes() {
        let mut out = Vec::new();
        note(&mut out, "CORE", NT_ARM_SYSTEM_CALL, &[1, 2, 3]);
        // namesz 5、descsz 3、类型，"CORE\0" 补到 8，内容补到 4
        assert_eq!(out.len(), 12 + 8 + 4);
        assert_eq!(&out[..4], &5u32.to_le_bytes());
        assert_eq!(&out[4..8], &3u32.to_le_bytes());
        assert_eq!(&out[12..17], b"CORE\0");
        assert_eq!(&out[20..24], &[1, 2, 3, 0]);
    }

    #[test]
    fn elf_header_places_segments_after_the_notes() {
        let regions = vec![
            DumpRegion {
                start: 0x10000,
                end: 0x12000,
                prot: prot::READ | prot::EXEC,
                size: 0x1000,
                sparse: false,
                file: Some((0, std::rc::Rc::from(&b"/bin/x"[..]))),
            },
            DumpRegion {
                start: 0x20000,
                end: 0x23000,
                prot: prot::READ | prot::WRITE,
                size: 0x3000,
                sparse: true,
                file: None,
            },
        ];
        let notes = vec![0u8; 100];
        let (h, data) = elf_header(&regions, &notes);
        assert_eq!(&h[..4], b"\x7fELF");
        assert_eq!(u16::from_le_bytes([h[16], h[17]]), ET_CORE);
        assert_eq!(u16::from_le_bytes([h[18], h[19]]), EM_AARCH64);
        assert_eq!(u16::from_le_bytes([h[56], h[57]]), 3);
        assert_eq!(data, PAGE_SIZE);
        assert_eq!(h.len() as u64, 64 + 3 * 56 + 100);
        // 第二个 PT_LOAD 紧接着第一个的内容，标志是 RW
        let ph = |i: usize, off: usize| {
            let at = 64 + i * 56 + off;
            u64::from_le_bytes(h[at..at + 8].try_into().unwrap())
        };
        assert_eq!(ph(1, 8), PAGE_SIZE);
        assert_eq!(ph(2, 8), PAGE_SIZE + 0x1000);
        assert_eq!(ph(2, 16), 0x20000);
        assert_eq!(ph(2, 40), 0x3000);
        assert_eq!(h[64 + 2 * 56 + 4], 6);
        let file = files(&regions);
        assert_eq!(&file[..8], &1u64.to_le_bytes());
        assert!(file.ends_with(b"/bin/x\0"));
    }
}
