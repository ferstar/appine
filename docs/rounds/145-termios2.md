# M9 之前：termios2（2026-10-31）

← [M9 之前：openat2（2026-10-31）](144-openat2.md) · [索引](../milestones.md)

第 144 轮之后筛兼容性推迟项时读 glibc 的源码发现：glibc 2.42 起 `tcgetattr` 只用 `TCGETS2`（`struct termios2`，多了输入、输出速率），失败了不退回 `TCGETS`；`isatty` 就是 `tcgetattr(fd) == 0`；`tcsetattr` 在 `__ASSUME_TERMIOS2`（arm64 上总是定义）时只用 `TCSETS2`、`TCSETSW2`、`TCSETSF2`。appine 的终端没有这几个 ioctl（ENOTTY），glibc ≥ 2.42 的发行版（Ubuntu 25.10、26.04 LTS，Fedora 43 起，Arch，Debian 14）里每个终端都"不是终端"。App 里现在的三个发行版不受影响（Alpine 是 musl，Debian 13 是 glibc 2.41，Ubuntu 24.04 是 2.39），所以历次普查都没碰到；这一条以前也没记进推迟项。

拿 Ubuntu 26.04.1 LTS（glibc 2.43，docker 的 `ubuntu:26.04` 导出成仓库）在 `--pty` 下跑 `bash -i` 确认过：改之前 `tty` 打印 "not a tty"，`stty size` 是 "Inappropriate ioctl for device"，readline 不启用（没有括号粘贴的开关序列）；改之后 `/dev/pts/0`、`24 80`。

## 做法

- **termios 带上速率**（`tty::Termios` 照内核的 `ktermios`，即 asm-generic 的 `struct termios2`，44 字节）：`TCGETS` 给前 36 字节，`TCGETS2` 给整个。PTY 的初始速率是 38400（`pts_driver->init_termios`）。
- **设置**（照 6.1 `tty_ioctl.c` 的 `set_termios`）：在原来的设置上换掉给出的部分，termio 只换标志的低 16 位和前 8 个控制字符，`TCSETS` 系列不换速率字段，`TCSETS2` 系列整个换；之后按 `CBAUD`、`CIBAUD` 重算两个速率（`tty_termios_baud_rate`、`tty_termios_input_baud_rate`）：`Bxxx` 按表换算（`CBAUDEX` 加 1–15 是 B57600 到 B4000000），`BOTHER` 用字段里的值，输入速率的几位是 0 时与输出相同。所以老的写法设置之后速率字段也总是对的，`TCSETS` 带着 `BOTHER` 时沿用原来的速率。`TCSETSF2` 与 `TCSETSF` 一样先清掉没读的输入，`TCSETSW2` 照常。
- **PTY 的硬件位**（`Tty::set_termios`）：与 `pty_set_termios` 一样总是 `CS8`、`CREAD`、不带 `PARENB`；与 `tty_set_termios` 一样地址位 `ADDRB` 改不了（只能经由驱动的 RS-485 配置改）。这两处以前就和 Linux 不同（设成 `CS7` 读回来还是 `CS7`），差分测试一起比了。
- `appine-run` 交给 guest 的宿主终端（`--no-pty` 时）也透传这四个 ioctl。

## 测试

- 新的差分测试组 `pty`（第 146 轮的包模式也放在这一组）：主设备、从设备的 `TCGETS2`，`TCGETS` 是它的前 36 字节；管道、`/dev/null`、目录上是 ENOTTY，坏的指针是 EFAULT；B9600、B115200 配 B1200 输入、B4000000、B0、`BOTHER`（输入输出不同、只有一边）设置之后的速率，字段里给的值在 `Bxxx` 时不算；经由主设备设置；老的 `TCSETS` 带 `BOTHER`、`TCSETA` 之后的速率；`CS6`、`PARENB`、`PARODD`、`ADDRB` 读回来的样子；别的标志、控制字符原样保存；`TCSETSF2` 清掉输入、`TCSETSW2` 不清；切到非规范模式时已有的输入保留；Python 的 `termios.tcsetattr`（musl）设 B57600 之后 `TCGETS2` 的速率；`isatty`。两个执行层下都与原生逐字相同。原生上写进主设备的数据经由 flip buffer 异步交给行规程，用例先等从设备可读再查 FIONREAD。
- 变异检查：设置之后不重算速率，用例在 B9600 那一行起不同。
- 单元测试：`speeds_follow_the_baud_bits`、`pty_keeps_eight_bits_and_the_address_bit`（`tty`）。
- Ubuntu 26.04 的仓库在 `target/glibc/ubuntu26-store`（`target/glibc/ubuntu-26-04.tar.gz` 导入），见上。
- cargo test（282 个）、clippy（含 Android）、格式、两层的全部差分测试通过。LTP 的 syscall 测试 1010 个通过，与上次逐项相同（sigtimedwait01、sigwaitinfo01 按 e4c03b9 记作 SKIP）。

**推迟项**：

- `TIOCGLCKTRMIOS`、`TIOCSLCKTRMIOS`（锁定 termios 的某些位）、`TIOCGSOFTCAR`、`TIOCSSOFTCAR` 仍是 ENOTTY：getty、setserial 这类管串口的程序才用。
- 速率只是记下、读回，不影响任何东西（PTY 本来没有速率，Linux 上也一样）。
