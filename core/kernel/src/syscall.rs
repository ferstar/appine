//! syscall 分发（设计文档 §7.5）。
//!
//! M0 只实现 `write`、`exit`、`exit_group`；其余 syscall 一律返回 `-ENOSYS`，
//! 绝不崩溃。

use appine_engine::Cpu;

use crate::debug;
use crate::errno::Errno;
use crate::mm::Mm;
use crate::uaccess::UserSlice;

const SYS_WRITE: u64 = 64;
const SYS_EXIT: u64 = 93;
const SYS_EXIT_GROUP: u64 = 94;

/// 单次从 guest 拷贝到宿主的最大字节数，避免按 guest 给的长度一次性分配内存。
const IO_CHUNK: usize = 64 * 1024;

/// syscall 处理完之后，运行循环该怎么继续。
pub(crate) enum Flow {
    /// 返回值已写入 x0，继续执行 guest。
    Continue,
    /// guest 请求退出，参数是退出码。
    Exit(i32),
}

/// 按 aarch64 Linux 的约定处理一次 `svc #0`：x8 是编号，x0–x5 是参数，返回值写回 x0。
pub(crate) fn dispatch(cpu: &mut Cpu, mm: &Mm) -> Flow {
    let state = cpu.state_mut();
    state.orig_x0 = state.x[0];
    let nr = state.x[8];
    let a = [state.x[0], state.x[1], state.x[2]];

    let result = match nr {
        SYS_WRITE => sys_write(mm, a[0], UserSlice::new(a[1], a[2] as usize)),
        SYS_EXIT | SYS_EXIT_GROUP => return Flow::Exit((a[0] & 0xff) as i32),
        _ => {
            debug::unimplemented_syscall(nr);
            Err(Errno::ENOSYS)
        }
    };
    cpu.state_mut().x[0] = match result {
        Ok(v) => v,
        Err(errno) => errno.to_syscall_return(),
    };
    Flow::Continue
}

fn sys_write(mm: &Mm, fd: u64, buf: UserSlice) -> Result<u64, Errno> {
    // M0 还没有 fd 表：guest 的 0/1/2 直接对应宿主的 0/1/2
    let host_fd = match fd {
        0..=2 => fd as i32,
        _ => return Err(Errno::EBADF),
    };
    let mut chunk = vec![0u8; buf.len.min(IO_CHUNK)];
    let mut written = 0;
    while written < buf.len {
        let n = (buf.len - written).min(IO_CHUNK);
        let part = UserSlice::new(buf.addr.wrapping_add(written as u64), n);
        let step = mm
            .read(part, &mut chunk[..n])
            .and_then(|()| appine_hal::write(host_fd, &chunk[..n]).map_err(Errno));
        match step {
            Ok(k) => {
                written += k;
                if k < n {
                    break;
                }
            }
            // 已经写出一部分时，像 Linux 一样返回已写的字节数
            Err(_) if written > 0 => break,
            Err(errno) => return Err(errno),
        }
    }
    Ok(written as u64)
}
