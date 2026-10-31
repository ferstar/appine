//! 内核日志（设计文档 §7.8）：Linux 6.1 的 printk 环形缓冲区。
//!
//! 记录由内核层自己（OOM killer 结束进程、内部错误结束进程，见 [`printk`]）和 guest 写
//! `/dev/kmsg` 产生。读的有三种，与 Linux 相同：
//!
//! - `/dev/kmsg`：每个打开文件有自己的位置（序号），一次读一条记录，格式是
//!   `前缀,序号,微秒,标志;正文`，不可打印的字符转义成 `\xNN`（[`Klog::ext_text`]）；
//! - syslog(2) 的读全部（`dmesg`）：从上次清空之后的记录里取放得下的最新的那些，每行前面加
//!   `<前缀>[秒.微秒] `（[`Klog::syslog_text`]），不改变任何状态；
//! - syslog(2) 的读、`/proc/kmsg`：系统里共用一个位置（`syslog_seq`），读走就没了，放不下
//!   的记录下次接着读（`syslog_partial`）。
//!
//! 缓冲区与 `CONFIG_LOG_BUF_SHIFT=17` 一样是 128 KB，最多 4096 条记录（`PRB_AVGBITS`），
//! 满了丢掉最老的。时间戳是宿主的 `CLOCK_MONOTONIC`（Linux 的 `local_clock`，不含休眠）。
//! 与 Linux 6.1 的环形缓冲区一样，结尾没有换行的记录还没定稿（等着 `KERN_CONT` 接着写），
//! 读的人看不到它，下一条记录来了才看得到。
//!
//! 内核日志在 [`crate::kernel::Kernel::klog`] 里，与别的内核对象一样只在持有 BKL 时访问；
//! `/dev/kmsg` 的打开文件拿着它（[`KmsgFile`]），poll、关闭时不必找内核。

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::rc::Rc;

use crate::cell::RefCell;
use crate::kernel::{request_wake_object, wake};

/// `log_buf_len`：`CONFIG_LOG_BUF_SHIFT` 的默认值 17。
pub(crate) const LOG_BUF_LEN: usize = 1 << 17;
/// 描述符的个数：`log_buf_len` 按平均每条 32 字节（`PRB_AVGBITS` = 5）算。
const MAX_RECORDS: usize = LOG_BUF_LEN >> 5;
/// 一条记录正文的上限（`LOG_LINE_MAX`，没有 `CONFIG_PRINTK_CALLER` 时前缀最长 32）。
/// `/dev/kmsg` 一次写超过它是 EINVAL；写满它的，存下来的正文还要少一个字节（`vsnprintf`
/// 的结尾 NUL 也算在里面）。
pub(crate) const LOG_LINE_MAX: usize = 1024 - 32;
/// syslog 格式一条记录（含每行的前缀）的上限（`CONSOLE_LOG_MAX`），放不下的行丢掉。
const CONSOLE_LOG_MAX: usize = 1024;
/// 记录的标志：结尾有换行（存的时候去掉了）、是接着上一条的（`KERN_CONT`）。
const LOG_NEWLINE: u8 = 2;
const LOG_CONT: u8 = 8;
/// Linux 的 `CLOCK_MONOTONIC`。
const CLOCK_MONOTONIC: i32 = 1;

/// 日志级别（`KERN_*`）。
pub(crate) mod level {
    pub const ERR: u8 = 3;
    pub const WARNING: u8 = 4;
    pub const INFO: u8 = 6;
}

/// `/proc/sys/kernel/printk_devkmsg`：guest 写 `/dev/kmsg` 记不记、限不限速。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Devkmsg {
    On,
    Off,
    Ratelimit,
}

impl Devkmsg {
    pub fn name(self) -> &'static str {
        match self {
            Devkmsg::On => "on",
            Devkmsg::Off => "off",
            Devkmsg::Ratelimit => "ratelimit",
        }
    }
}

/// 一条记录。
struct Record {
    seq: u64,
    ts_nsec: u64,
    facility: u8,
    level: u8,
    flags: u8,
    text: Box<[u8]>,
}

impl Record {
    /// 在数据环里占的字节数：块的编号（8 字节）加正文，按 8 字节对齐。
    fn data_size(&self) -> usize {
        (8 + self.text.len()).div_ceil(8) * 8
    }

    /// 行数：正文里的换行数加一（存的时候结尾的换行去掉了）。
    fn lines(&self) -> usize {
        self.text.iter().filter(|&&b| b == b'\n').count() + 1
    }

    /// syslog 格式每行的前缀（`info_print_prefix`）。
    fn prefix(&self, syslog: bool, time: bool) -> String {
        let mut s = String::new();
        if syslog {
            write!(s, "<{}>", (self.facility as u32) << 3 | self.level as u32).unwrap();
        }
        if time {
            let (sec, ns) = (self.ts_nsec / 1_000_000_000, self.ts_nsec % 1_000_000_000);
            write!(s, "[{sec:5}.{:06}] ", ns / 1000).unwrap();
        }
        s
    }
}

/// 内核日志。
pub(crate) struct Klog {
    records: VecDeque<Record>,
    /// 记录在数据环里一共占的字节数（[`Record::data_size`]）。
    data: usize,
    /// 下一条记录的序号。
    next_seq: u64,
    /// syslog 的清空（`dmesg -c`）之后的第一条。
    clear_seq: u64,
    /// syslog 的读、`/proc/kmsg` 共用的位置，和这条记录已经读走的字节数。
    syslog_seq: u64,
    syslog_partial: usize,
    /// 读这条记录的开头时带不带时间（`syslog_time`）：读到一半改了设置也接着用原来的。
    syslog_time: bool,
    /// `/sys/module/printk/parameters/time`（`CONFIG_PRINTK_TIME=y`）。
    pub printk_time: bool,
    pub devkmsg: Devkmsg,
    /// 只有 `CAP_SYS_ADMIN` 的进程读日志时的警告已经记过了（`pr_warn_once`）。
    pub sys_admin_warned: bool,
}

impl Default for Klog {
    fn default() -> Klog {
        Klog {
            records: VecDeque::new(),
            data: 0,
            next_seq: 0,
            clear_seq: 0,
            syslog_seq: 0,
            syslog_partial: 0,
            syslog_time: true,
            printk_time: true,
            devkmsg: Devkmsg::Ratelimit,
            sys_admin_warned: false,
        }
    }
}

/// 读出的一条记录（拷贝出来，放开借用之后再写给 guest）。
pub(crate) struct Read {
    /// 记录的序号；比要求的大说明要的那条已经被挤掉了。
    pub seq: u64,
    pub text: Vec<u8>,
}

impl Klog {
    /// 存一条记录（`vprintk_store`）：正文到第一个 NUL 为止（`%s`），最多
    /// `LOG_LINE_MAX - 1` 个字节，结尾的一个换行去掉、记成标志。不叫醒读的线程，见
    /// [`emit`]。
    fn store(&mut self, facility: u8, level: u8, text: &[u8]) {
        let text = &text[..text.iter().position(|&b| b == 0).unwrap_or(text.len())];
        let mut text = &text[..text.len().min(LOG_LINE_MAX - 1)];
        let mut flags = 0;
        if let Some(rest) = text.strip_suffix(b"\n") {
            text = rest;
            flags |= LOG_NEWLINE;
        }
        let ts_nsec = appine_hal::clock_gettime(CLOCK_MONOTONIC)
            .map_or(0, |(s, ns)| s as u64 * 1_000_000_000 + ns as u64);
        let record = Record {
            seq: self.next_seq,
            ts_nsec,
            facility,
            level: level & 7,
            flags,
            text: text.into(),
        };
        self.next_seq += 1;
        self.data += record.data_size();
        self.records.push_back(record);
        while self.data > LOG_BUF_LEN || self.records.len() > MAX_RECORDS {
            let old = self.records.pop_front().expect("the newest record fits");
            self.data -= old.data_size();
        }
    }

    /// 最老的一条的序号（`prb_first_valid_seq`）；没有记录时是下一条的。
    pub fn first_seq(&self) -> u64 {
        self.records.front().map_or(self.next_seq, |r| r.seq)
    }

    /// 读得到的记录数：最新的一条结尾没有换行时还没定稿（`desc_make_final` 要等下一条
    /// 记录），不算。
    fn visible(&self) -> usize {
        let open = self
            .records
            .back()
            .is_some_and(|r| r.flags & LOG_NEWLINE == 0);
        self.records.len() - open as usize
    }

    /// 读得到的、序号不小于 `seq` 的记录。
    fn readable_from(&self, seq: u64) -> impl Iterator<Item = &Record> {
        self.records
            .iter()
            .take(self.visible())
            .filter(move |r| r.seq >= seq)
    }

    /// syslog 上次清空之后的第一条（`/dev/kmsg` 的 `SEEK_DATA`）。
    pub fn clear_seq(&self) -> u64 {
        self.clear_seq
    }

    /// 读得到的最后一条之后的序号（`prb_next_seq`）：也是读得到过几条，poll 的数据代数
    /// 用它。
    pub fn next_seq(&self) -> u64 {
        self.first_seq() + self.visible() as u64
    }

    /// 读得到的、序号不小于 `seq` 的第一条（`prb_read_valid`）。
    fn record(&self, seq: u64) -> Option<&Record> {
        let index: usize = seq
            .saturating_sub(self.first_seq())
            .try_into()
            .unwrap_or(usize::MAX);
        self.records.get(index).filter(|_| index < self.visible())
    }

    /// 序号不小于 `seq` 的记录在不在：读的人等的就是它。
    pub fn readable(&self, seq: u64) -> bool {
        self.record(seq).is_some()
    }

    /// `/dev/kmsg` 读到的格式（`info_print_ext_header`、`msg_print_ext_body`）：序号不小于
    /// `seq` 的第一条。
    pub fn ext_text(&self, seq: u64) -> Option<Read> {
        let r = self.record(seq)?;
        let mut text = format!(
            "{},{},{},{};",
            (r.facility as u32) << 3 | r.level as u32,
            r.seq,
            r.ts_nsec / 1000,
            if r.flags & LOG_CONT != 0 { 'c' } else { '-' }
        )
        .into_bytes();
        for &c in &*r.text {
            if !(b' '..127).contains(&c) || c == b'\\' {
                text.extend_from_slice(format!("\\x{c:02x}").as_bytes());
            } else {
                text.push(c);
            }
        }
        text.push(b'\n');
        Some(Read { seq: r.seq, text })
    }

    /// syslog 格式的一条（`record_print_text`，缓冲区是 `CONSOLE_LOG_MAX`）：每行前面加
    /// 前缀，结尾补上换行；加了前缀放不下的行从后往前丢掉，第一行放不下就截短它。
    fn syslog_text(r: &Record, time: bool) -> Vec<u8> {
        let prefix = r.prefix(true, time);
        let prefix = prefix.as_bytes();
        let mut out = Vec::new();
        let mut text: &[u8] = &r.text;
        let mut truncated = false;
        loop {
            let next = text.iter().position(|&b| b == b'\n');
            if next.is_none() && truncated {
                break;
            }
            let line_len = next.unwrap_or(text.len());
            let mut text_len = text.len();
            if out.len() + prefix.len() + text_len + 2 > CONSOLE_LOG_MAX {
                if out.len() + prefix.len() + line_len + 2 > CONSOLE_LOG_MAX {
                    break;
                }
                text_len = CONSOLE_LOG_MAX - out.len() - prefix.len() - 2;
                truncated = true;
            }
            out.extend_from_slice(prefix);
            out.extend_from_slice(&text[..line_len.min(text_len)]);
            out.push(b'\n');
            if text_len == line_len {
                break;
            }
            text = &text[line_len + 1..text_len];
        }
        out
    }

    /// [`Klog::syslog_text`] 不截短时的长度（`get_record_print_text_size`）。
    fn syslog_size(r: &Record, time: bool) -> usize {
        r.prefix(true, time).len() * r.lines() + r.text.len() + 1
    }

    /// syslog 的读全部（`syslog_print_all`）：从清空之后的记录里，取放得进 `size` 个字节的
    /// 最新的那些。`clear` 时（读后清空）之后从读到的后面开始。
    pub fn read_all(&mut self, size: usize, clear: bool) -> Vec<u8> {
        let time = self.printk_time;
        let start = self.clear_seq.max(self.first_seq());
        let from = |seq: u64| self.readable_from(seq);
        let mut len: usize = from(start).map(|r| Self::syslog_size(r, time)).sum();
        let mut seq = self.next_seq();
        for r in from(start) {
            if len <= size {
                seq = r.seq;
                break;
            }
            len -= Self::syslog_size(r, time);
        }
        let mut out = Vec::new();
        let mut end = self.next_seq();
        for r in from(seq) {
            let text = Self::syslog_text(r, time);
            if out.len() + text.len() > size {
                // Linux 这里是 seq--：读后清空时，清空的位置停在放不下的前一条
                end = r.seq.saturating_sub(1);
                break;
            }
            out.extend_from_slice(&text);
        }
        if clear {
            self.clear_seq = end;
        }
        out
    }

    /// syslog 的清空（`syslog_clear`）。
    pub fn clear(&mut self) {
        self.clear_seq = self.next_seq();
    }

    /// syslog 的读、`/proc/kmsg` 有没有读得到的记录（等的条件）。
    pub fn syslog_readable(&self) -> bool {
        self.readable(self.syslog_seq)
    }

    /// syslog 的读（`syslog_print`），已经有记录可读：读走放得进 `size` 个字节的记录，第一
    /// 条放不下时读走它的前一部分，下次接着读。
    pub fn syslog_read(&mut self, size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < size {
            let Some(seq) = self.record(self.syslog_seq).map(|r| r.seq) else {
                break;
            };
            if seq != self.syslog_seq {
                self.syslog_seq = seq;
                self.syslog_partial = 0;
            }
            if self.syslog_partial == 0 {
                self.syslog_time = self.printk_time;
            }
            let r = self.record(seq).expect("the record was just found");
            let text = Self::syslog_text(r, self.syslog_time);
            let skip = self.syslog_partial;
            let room = size - out.len();
            let n = if text.len() - skip <= room {
                self.syslog_seq = seq + 1;
                self.syslog_partial = 0;
                text.len() - skip
            } else if out.is_empty() {
                self.syslog_partial += room;
                room
            } else {
                0
            };
            if n == 0 {
                break;
            }
            out.extend_from_slice(&text[skip..skip + n]);
        }
        out
    }

    /// syslog 的未读字节数（`SYSLOG_ACTION_SIZE_UNREAD`）；`/proc/kmsg` 用它时（`from_proc`）
    /// 是未读的记录数。挤掉了的记录先跳过去。
    pub fn size_unread(&mut self, from_proc: bool) -> u64 {
        let Some(r) = self.record(self.syslog_seq) else {
            return 0;
        };
        if r.seq != self.syslog_seq {
            self.syslog_seq = r.seq;
            self.syslog_partial = 0;
        }
        if from_proc {
            return self.next_seq() - self.syslog_seq;
        }
        let mut time = if self.syslog_partial > 0 {
            self.syslog_time
        } else {
            self.printk_time
        };
        let mut n = 0;
        for r in self.readable_from(self.syslog_seq) {
            n += Self::syslog_size(r, time);
            time = self.printk_time;
        }
        (n - self.syslog_partial) as u64
    }
}

/// 记一条内核层自己的消息（`printk`，facility 0），叫醒等日志的线程。与内核里的消息一样
/// 以换行结尾（马上就读得到）。日志正被借用时（在日志自己的代码里出了错）丢掉这条。调用方
/// 持有 BKL。
pub(crate) fn printk(log: &RefCell<Klog>, level: u8, args: fmt::Arguments<'_>) {
    let mut text = fmt::format(args);
    text.push('\n');
    emit(log, 0, level, text.as_bytes());
}

/// 记一条记录（`vprintk_emit`），叫醒等日志的线程（Linux 的 `log_wait`）。
pub(crate) fn emit(log: &RefCell<Klog>, facility: u8, level: u8, text: &[u8]) {
    if let Ok(mut l) = log.try_borrow_mut() {
        l.store(facility, level, text);
        request_wake_object(wake::of(log));
    }
}

/// 一个 `/dev/kmsg` 的打开文件（`struct devkmsg_user`）：读的位置和写的限速。
pub(crate) struct KmsgFile {
    pub log: Rc<RefCell<Klog>>,
    /// 下一次读的序号：打开时是最老的一条。
    pub seq: u64,
    rs: Ratelimit,
}

impl KmsgFile {
    pub fn new(log: Rc<RefCell<Klog>>) -> KmsgFile {
        let seq = log.borrow().first_seq();
        KmsgFile {
            log,
            seq,
            rs: Ratelimit::default(),
        }
    }

    /// 写一条（`devkmsg_write`）：开头的 `<数字>` 是 syslog 的前缀，低三位是级别，其余是
    /// facility（0 时仍当作 `LOG_USER`）；没有前缀时是默认的消息级别、`LOG_USER`。
    /// 限速（`printk_devkmsg` 是 `ratelimit` 时每 5 秒最多 10 条）丢掉的照样算写成功。
    /// 长度已经检查过了（不超过 [`LOG_LINE_MAX`]）。
    pub fn write(&mut self, data: &[u8], default_level: i32) {
        let devkmsg = self.log.borrow().devkmsg;
        if devkmsg == Devkmsg::Off
            || (devkmsg == Devkmsg::Ratelimit && !self.rs.allow(std::time::Instant::now()))
        {
            return;
        }
        let (mut level, mut facility) = (default_level as u8 & 7, 1);
        let mut line = data;
        if let Some(rest) = data.strip_prefix(b"<") {
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            if rest.get(digits) == Some(&b'>') {
                // simple_strtoul 的结果截成 unsigned int
                let u = rest[..digits].iter().fold(0u64, |v, &d| {
                    v.wrapping_mul(10).wrapping_add((d - b'0') as u64)
                }) as u32;
                level = (u & 7) as u8;
                if (u >> 3) & 0xff != 0 {
                    facility = ((u >> 3) & 0xff) as u8;
                }
                line = &rest[digits + 1..];
            }
        }
        emit(&self.log, facility, level, line);
    }
}

impl Drop for KmsgFile {
    /// 关闭时报告被限速丢掉了几条（`ratelimit_state_exit`，带着关闭它的进程的名字）。
    fn drop(&mut self) {
        if self.rs.missed > 0 {
            // 打开文件总在持有 BKL 时释放；单元测试里没有 BKL，也没有当前线程
            let current = crate::kernel::bkl_held()
                .then(crate::task::current)
                .flatten();
            let comm = current.map_or_else(String::new, |(t, _)| {
                crate::pseudo::comm_str(Some(&*t), t.tgid)
            });
            printk(
                &self.log,
                level::WARNING,
                format_args!(
                    "printk: {comm}: {} output lines suppressed due to ratelimiting",
                    self.rs.missed
                ),
            );
        }
    }
}

/// `struct ratelimit_state`，默认的间隔 5 秒、最多 10 次，`RATELIMIT_MSG_ON_RELEASE`：
/// 被丢掉的次数在关闭时才报告。
#[derive(Default)]
struct Ratelimit {
    begin: Option<std::time::Instant>,
    printed: u32,
    missed: u32,
}

impl Ratelimit {
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    const BURST: u32 = 10;

    /// `___ratelimit`：这一次放不放行。
    fn allow(&mut self, now: std::time::Instant) -> bool {
        let begin = *self.begin.get_or_insert(now);
        if now > begin + Self::INTERVAL {
            self.begin = Some(now);
            self.printed = 0;
        }
        if self.printed < Self::BURST {
            self.printed += 1;
            true
        } else {
            self.missed += 1;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每条都加上结尾的换行，读得到。
    fn log_with(texts: &[&[u8]]) -> Klog {
        let mut l = Klog::default();
        for t in texts {
            l.store(0, 6, &[t, &b"\n"[..]].concat());
        }
        l
    }

    #[test]
    fn records_strip_one_newline_and_stop_at_nul() {
        let mut l = Klog::default();
        for t in [&b"a\n\n"[..], b"b\0c\n", b""] {
            l.store(0, 6, t);
        }
        let texts: Vec<&[u8]> = l.records.iter().map(|r| &*r.text).collect();
        assert_eq!(texts, [&b"a\n"[..], b"b", b""]);
        assert_eq!(l.records[0].flags, LOG_NEWLINE);
        assert_eq!(l.records[1].flags, 0);
    }

    #[test]
    fn a_record_without_a_newline_is_read_after_the_next_one() {
        let mut l = Klog::default();
        l.store(1, 4, b"open");
        assert_eq!((l.first_seq(), l.next_seq()), (0, 0));
        assert!(!l.readable(0) && l.ext_text(0).is_none());
        assert_eq!(l.read_all(4096, false), b"");
        l.store(1, 4, b"closed\n");
        assert_eq!(l.next_seq(), 2);
        assert!(l.ext_text(0).unwrap().text.ends_with(b";open\n"));
    }

    #[test]
    fn ext_text_escapes_and_reports_the_next_valid_record() {
        let l = log_with(&[b"x\\y\x01\xff"]);
        let r = l.ext_text(0).unwrap();
        let s = String::from_utf8(r.text).unwrap();
        assert!(s.starts_with("6,0,"), "{s}");
        assert!(s.ends_with(",-;x\\x5cy\\x01\\xff\n"), "{s}");
        assert!(l.ext_text(1).is_none());
    }

    #[test]
    fn old_records_are_dropped_when_the_buffer_is_full() {
        let mut l = Klog::default();
        let mut line = [b'x'; 900];
        line[899] = b'\n';
        for _ in 0..200 {
            l.store(1, 4, &line);
        }
        assert!(l.data <= LOG_BUF_LEN);
        assert_eq!(l.next_seq(), 200);
        assert!(l.first_seq() > 0);
        // 被挤掉的序号读到的是最老的那条
        assert_eq!(l.ext_text(0).unwrap().seq, l.first_seq());
        for i in 0..5000 {
            l.store(1, 4, format!("{i}\n").as_bytes());
        }
        assert_eq!(l.records.len(), MAX_RECORDS);
    }

    #[test]
    fn syslog_lines_get_prefixes_and_long_records_lose_lines() {
        let mut l = log_with(&[b"one\ntwo"]);
        l.printk_time = false;
        assert_eq!(l.read_all(4096, false), b"<6>one\n<6>two\n");
        // 第二行加上前缀放不下 1024 字节，丢掉
        let mut text = vec![b'a'; 600];
        text.push(b'\n');
        text.extend(vec![b'b'; 600]);
        let r = Record {
            seq: 0,
            ts_nsec: 0,
            facility: 0,
            level: 6,
            flags: 0,
            text: text.into(),
        };
        let out = Klog::syslog_text(&r, false);
        assert_eq!(out.len(), 3 + 600 + 1);
    }

    #[test]
    fn read_all_keeps_the_newest_records_that_fit_and_clear_moves_on() {
        let mut l = log_with(&[b"first", b"second", b"third"]);
        l.printk_time = false;
        assert_eq!(l.read_all(20, false), b"<6>second\n<6>third\n");
        assert_eq!(l.read_all(4096, true).len(), 9 + 10 + 9);
        assert_eq!(l.read_all(4096, false), b"");
        l.store(0, 3, b"later\n");
        assert_eq!(l.read_all(4096, false), b"<3>later\n");
    }

    #[test]
    fn syslog_read_consumes_and_continues_partial_records() {
        let mut l = log_with(&[b"abcdef", b"gh"]);
        l.printk_time = false;
        assert_eq!(l.size_unread(false), 10 + 6);
        assert_eq!(l.size_unread(true), 2);
        assert_eq!(l.syslog_read(4), b"<6>a");
        assert_eq!(l.size_unread(false), 6 + 6);
        // 剩下的部分放得下，下一条放不下：只读到这条的结尾
        assert_eq!(l.syslog_read(10), b"bcdef\n");
        assert_eq!(l.syslog_read(100), b"<6>gh\n");
        assert!(!l.syslog_readable());
        assert_eq!(l.size_unread(false), 0);
    }

    #[test]
    fn devkmsg_prefixes_and_ratelimit() {
        let log = Rc::new(RefCell::new(Klog::default()));
        let mut f = KmsgFile::new(log.clone());
        f.write(b"<11>user err\n", 4);
        f.write(b"<0>kernel facility stays user\n", 4);
        f.write(b"<x>not a prefix\n", 4);
        f.write(b"<>empty prefix\n", 4);
        {
            let l = log.borrow();
            let meta: Vec<(u8, u8, &[u8])> = l
                .records
                .iter()
                .map(|r| (r.facility, r.level, &*r.text))
                .collect();
            assert_eq!(
                meta,
                [
                    (1, 3, &b"user err"[..]),
                    (1, 0, b"kernel facility stays user"),
                    (1, 4, b"<x>not a prefix"),
                    (1, 0, b"empty prefix"),
                ]
            );
        }
        for _ in 0..10 {
            f.write(b"more\n", 4);
        }
        // 前 10 条放行，后面的丢掉，关闭时报告丢了几条
        assert_eq!(log.borrow().next_seq(), 10);
        drop(f);
        let l = log.borrow();
        let last = l.records.back().unwrap();
        assert_eq!(last.level, level::WARNING);
        assert!(
            last.text
                .ends_with(b": 4 output lines suppressed due to ratelimiting")
        );
    }

    #[test]
    fn ratelimit_window_restarts() {
        let mut rs = Ratelimit::default();
        let t = std::time::Instant::now();
        assert!((0..10).all(|_| rs.allow(t)));
        assert!(!rs.allow(t));
        assert!(rs.allow(t + Ratelimit::INTERVAL + std::time::Duration::from_millis(1)));
        assert_eq!(rs.missed, 1);
    }
}
