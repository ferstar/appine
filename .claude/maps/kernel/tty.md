# kernel/tty Map
> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/tty.md` for the paired decision history.
> Verified: 2026-11-03 (full mapping: 3 research concerns; 14 claims checked by 2 independent verifiers — 13 confirmed and 1 partial, corrected below; bugs and debts recorded)

## Responsibilities

`kernel/tty` implements pseudo-terminals entirely inside the kernel layer:

- PTY pairs: allocation through `/dev/ptmx`, opening slaves, `/dev/tty` and `/dev/console`,
  `TIOCGPTPEER`, closing, hangup and `vhangup`;
- the line discipline (input mapping, canonical editing, echo, flow control, signal characters,
  output processing, `VMIN`/`VTIME`) and termios/termios2;
- packet mode;
- job control: controlling terminals, sessions and foreground groups, background-access checks,
  terminal-generated signals;
- the host console: kernel-held masters for the app's terminal tabs and the `appine-run` bridge.

Root: `core/kernel/src/tty.rs`, `syscall/tty.rs`, `terminal.rs`.

Outside the module: devpts as a filesystem, instances, and the owner, mode and times shown for
`/dev/pts/N` (`kernel/procfs`); device-number dispatch on open (`kernel/vfs`); signal delivery,
stops and orphaned-group stop rules (`kernel/signal`); `setsid`/`setpgid`, process exit and the
exit-time orphan signals (`kernel/process`); the Android terminal UI (`android`).

## Key types

- `Tty` — one PTY pair (shared by both sides): devpts device and index; `masters`/`slaves`
  counts, `slave_closed`, `locked`, `hung_up`, per-side `vhangups` generations and `hupped`;
  termios (the slave's, the only one), winsize; buffers `editing` (canonical line being edited),
  `lines` (finished lines; an empty line marks EOF), `raw` (non-canonical queue), `output`
  (waiting for the master); flow state (`held_from`, `tco_stopped`, `master_stopped`), packet state
  (`packet`, `pktstatus`), `lnext`; `session` and `fg`; owner, mode, times; edge counters
  `arrivals`.
- `PtyEnd` — one open end: the `Tty`, master flag, mount, `pin` (keeps the devpts mount busy),
  `born` (the side's vhangup generation at open). Opening and dropping it maintain the counts.
- `Termios` — kernel termios2 layout (four flag words, line, 19 control characters, input and
  output speed); `flags`, `cc`, `pkt` constants (a subset of Linux's); `Winsize`.
- `Kernel::ptys` — `(device, index) → Weak<Tty>`; the thread-local `CLOSED` list of terminals
  whose last master closed.
- `terminal::Console` (a kernel-held master and whether a session is attached) in
  `Kernel::consoles`; `terminal::Terminal` (host fds for the bridge) and its EOF state machine.
- `ThreadGroup.ctty` and `tty_old_pgrp` (in `kernel/process`'s structures) link processes to
  their controlling terminal.

## Public entry points

- Opening (from `kernel/vfs`'s device dispatch): `new_pty`, `open_ptmx`, `open_slave`,
  `open_peer` (`TIOCGPTPEER`), `open_ctty` (5:0 and 5:1).
- I/O: `syscall::tty::read`, `write`, `readiness`, `ioctl`.
- Hangup: `hang_up`, `vhangup`, `vhangup_ctty` (`vhangup(2)`), `hang_up_closed` (run when the BKL
  is released).
- Job control helpers: `signal_fg`, `check_background`.
- Host console: `terminal::{open, attach, wake_objects, take_output, input_room, feed, fg_cwd,
  resize, close, initial_size, bridge}`, wrapped by `Machine::{open_console, read_console,
  write_console, resize_console, close_console, console_cwd}` and `Stdio::Console`.

## Data flow / lifecycle

**1. Allocation and opening**

- `/dev/ptmx` → `new_pty`: drop dead table entries, read the instance's options, `ENOSPC` at
  `max`, take the lowest free index, owner from the options or the opener's fsuid/fsgid, mode
  from the options. A new terminal is locked with winsize 0×0; the master end makes `masters` 1.
- Master-only ioctls: `TIOCSPTLCK`/`TIOCGPTLCK`, `TIOCGPTN`, `TIOCPKT`/`TIOCGPKT`, `TIOCSIG`,
  `TIOCGPTPEER` (opens the slave through the master's mount, keeping access mode, `O_APPEND`,
  `O_NONBLOCK`, honouring `O_CLOEXEC`; on a slave or hung master `EIO`).
- Slave open: no live entry `ENOENT`; locked or no master `EIO`; without `O_NOCTTY` a session
  leader without a controlling terminal acquires an unowned terminal (section 5).
- `/dev/tty` and `/dev/console` both open the caller's controlling terminal as a slave end
  (`ENXIO` without one) and never acquire one.

**2. Input (master writes, host console feed)**

- The writer is clamped to `input_room`; each byte goes through `input_char`: `ISTRIP`; a pending
  `VLNEXT` makes it literal; `IGNCR`/`ICRNL`/`INLCR`; `IXON` (`VSTOP` stops output, `VSTART` or —
  with `IXANY` — any byte restarts it); `ISIG` (`VINTR`, `VQUIT`, `VSUSP` → signals, flushing
  input and output unless `NOFLSH`, echoing `^C`); `ICANON` editing (`VERASE`, `VWERASE`,
  `VKILL`, `VREPRINT`, `VEOF`, line ends `\n`/`VEOL`/`VEOL2`, `IUTF8` erasing whole characters)
  with `ECHO`, `ECHOE`, `ECHOK`, `ECHOKE`, `ECHOCTL`, `ECHONL`.
- Limits: input holds 4096 bytes; in canonical mode a full buffer still accepts one byte so a
  line end can get through, and other bytes past 4095 are dropped silently (no bell, no echo). A
  full queue blocks the writer, returns `EAGAIN` or a short count. `TIOCSTI` bypasses the room
  check, so in raw mode it can grow the queue without limit.
- Not implemented: `IUCLC`, `IMAXBEL`, `ECHOPRT`, `PARMRK`, `BRKINT`/`IGNBRK`, `HUPCL`,
  `VDISCARD`; `EXTPROC` only reports a packet-mode `IOCTL` and leaves local editing on.

**3. Reading on the slave**

- Canonical: one line per read; an empty line returns 0 (EOF). Non-canonical: `VMIN`/`VTIME`
  (0/0 returns at once; 0/T times out from the start of the read; N/0 waits for
  `min(VMIN, len)` bytes; N/T arms one deadline at the first byte and never re-arms it, a total
  rather than Linux's inter-byte timer). Slave `POLLIN` is set for any queued raw byte, whatever
  `VMIN`; a nonblocking read with fewer than `VMIN` bytes queued (and `VTIME` 0) returns `EAGAIN`.
- No data: `EIO` without a master, `EAGAIN` when nonblocking, otherwise wait on the terminal's
  wake object. The background check runs once before blocking (section 5). Reads update atime
  and writes mtime (seconds only, on 8-second boundaries).

**4. Output (slave writes, master reads)**

- Slave writes are clamped to `output_room` (0 while output is stopped, otherwise 64 KiB minus
  the queue), then `OPOST` with `ONLCR`/`OCRNL` (no `ONOCR`, `ONLRET`, `OLCUC` or tab expansion,
  no column tracking); `EIO` without a master; `TOSTOP` background check.
- `held_from` marks where output stopped: the master can read what came before; later output and
  echoes queue invisibly until restart; `send_xchar` inserts before the held part.
- Master reads: a pending packet status byte is returned alone; otherwise `TIOCPKT_DATA` (in
  packet mode) plus the visible output. Once the slave has been opened and all slaves closed,
  master reads return `EIO` after draining and poll reports `POLLHUP`; a never-opened slave does
  not count, so reads block.
- Packet status: `FLUSHREAD`/`FLUSHWRITE` (signal characters, flushes), `STOP`/`START` (each
  clears the other), `DOSTOP`/`NOSTOP` (when "default ^S/^Q flow" changes), `IOCTL` (with
  `EXTPROC`); enabling packet mode clears stale status; pending status adds `POLLPRI|POLLIN`.

**5. Job control**

- Acquire: the console's session leader gets the console at spawn; a session leader opening an
  unowned slave without `O_NOCTTY` acquires it (`session`, `fg` = its group); `TIOCSCTTY` returns
  0 if already the ctty, `EPERM` for non-leaders or callers with a ctty, and steals a terminal from
  another session only with argument 1 (clearing that session's links).
- Foreground: `TIOCSPGRP` needs the caller's ctty (`ENOTTY`), passes the background check (an
  orphaned caller's `EIO` is returned as is), then `EINVAL` for negative values, translates
  through the pid namespace, `EPERM` unless a group with that id exists in the caller's session
  (also for 0 and unknown ids); `TIOCGPGRP`/`TIOCGSID` need the caller's ctty and translate out
  (0 when invisible). Because both ends share one `Tty`, these work on a master fd only when that
  terminal is the caller's ctty, and `TIOCNOTTY` is accepted on a master fd.
- Background access (`check_background`, Linux's `__tty_check_change`): applies when the terminal
  is the caller's ctty and another group is in the foreground; an ignored or blocked signal makes
  reads `EIO` and lets writes proceed; an orphaned group gets `EIO`; otherwise the signal goes to
  the caller's group and the call returns `ERESTARTSYS`. Callers: slave reads, slave writes with
  `TOSTOP`, termios sets from the slave, `TIOCSPGRP` (not `TCFLSH`, `TCXONC`, `TCSBRK` or
  `TIOCSETD`).
- Signals: line-discipline signals, `TIOCSTI`, `TCXONC` on the master, the host `feed` and
  `TIOCSIG` (INT, QUIT, TSTP only) go to the foreground group; `TIOCSWINSZ` from either side and
  the host `resize` send `SIGWINCH` to it only when the size changes.
- Give up: `setsid` clears only the caller's link; `TIOCNOTTY` by a non-leader clears its own, by
  the leader clears the session's links and sends `SIGHUP` then `SIGCONT` to the foreground
  group; a session leader's exit with its ctty sends both to the foreground group and clears the
  session, without one it signals `tty_old_pgrp`.

**6. Hangup**

- Last master close: `PtyEnd::drop` cannot reach the kernel, so it queues the terminal on
  `CLOSED`; the thread releasing the BKL runs `hang_up_closed`, which marks it hung up, removes
  the table entry (the index can be reused, `/dev/pts/N` disappears), clears every process's link
  to it, records the foreground group as the leader's `tty_old_pgrp` and sends `SIGHUP` then
  `SIGCONT` to the session leader only.
- Hung slave ends: reads return 0 (pending input discarded), writes `EIO`, ioctls `EIO`
  (`TIOCSPGRP` `ENOTTY`), poll reports everything including `POLLERR`. Between the last master
  close and the deferred hangup, slave reads and writes return `EIO`.
- `vhangup(2)` (needs `CAP_SYS_TTY_CONFIG`, does nothing without a ctty) and `TIOCVHANGUP`
  (needs `CAP_SYS_ADMIN`) hang up one side by bumping its generation: every end opened earlier is
  hung; the slave side also flushes input, clears packet status and detaches the session; the node
  stays and can be reopened.

**7. termios ioctls**

- Either side: `TCGETS`/`TCGETS2`/`TCGETA`, all `TCSETS*`/`TCSETA*`/`TCSETS2*` (CS8 and CREAD
  forced, PARENB cleared; speeds recomputed and only recorded; the `W` forms do not drain, `F`
  forms flush slave input), `TIOCGWINSZ`/`TIOCSWINSZ`, `TCFLSH`, `TCXONC` (slave: TCOOFF/TCOON;
  master: stops input; 2/3 send VSTOP/VSTART), `TCSBRK`/`TCSBRKP` (no-ops), `TIOCEXCL`/`TIOCNXCL`
  (no-ops), `TIOCSETD`/`TIOCGETD` (always 0), `FIONREAD` (slave: complete lines; master: output
  queue including held bytes), `TIOCOUTQ`, `TIOCSTI`; unknown requests `ENOTTY`.
- Switching `ICANON` moves queued input between the canonical and raw queues (raw data becomes
  the editing line, readable only after a line end).
- Defaults match Linux's pts `init_termios` (`tty_std_termios` without `HUPCL`); a control
  character of 0 is disabled.

**8. Host console**

- `terminal::open` creates a terminal on the boot devpts instance (lowest free index, unlocked,
  the given size) held by the kernel; `attach` (from `spawn`) makes the new session leader its
  session and foreground and returns one slave file installed as fds 0–2 and the ctty (`EBUSY` if
  already attached).
- The host takes output (`take_output`: bytes, nothing yet, or empty once the attached session's
  slaves are all closed), feeds input under `input_room` backpressure (`feed` delivers resulting
  signals), resizes (`SIGWINCH` on change), asks for the foreground group's cwd (`fg_cwd`, for
  restoring tabs), and closes (drop the master, hang up immediately). `write_console` keeps
  queuing input after the attached session has ended and fails with `EIO` only once the queue is
  full. Console terminals are numbered without pruning dead table entries and take the default
  owner (root:tty 0620) rather than the instance options. Host threads take the BKL and
  wait on the terminal's wake object like guest threads.
- `bridge` (`appine-run`): raw mode on the host terminal when stdin and stdout are terminals,
  host `SIGWINCH` → resize; `--pty` on a non-terminal stdin uses 24×80 and no resizes. Output goes
  to the host; input is read only while there is room. At host EOF it waits until the program has
  no unread input, then sends `VEOF` (again if the program left canonical mode before reading
  it; nothing if `VEOF` is disabled), as util-linux `script` does.

## Dependencies (inbound / outbound)

**Inbound**

- `kernel/vfs` (open by device number, `devpts_beside`, read/write/ioctl dispatch, devpts
  `nodev`), poll/epoll (`readiness`, `arrivals`), `kernel/process` (fork copying the ctty, setsid,
  exit releasing the ctty), `Bkl::drop` (`hang_up_closed`), the machine and JNI/CLI hosts (console
  API, bridge), procfs (stat of `/dev/pts/N`, `tty_nr`/`tpgid`).

**Outbound**

- `kernel/signal` (`send_to_pgrp`, `send_to_group`, `pgrp_orphaned`, kernel siginfo), the kernel's
  wake objects and `wait_objects`, pid-namespace translation (`pid_in`/`pid_out`), the VFS
  (`devpts_options`, `path_of`), `appine_hal` (host terminal raw mode, `TIOCGWINSZ`, SIGWINCH
  notifier, read/write, clock).

## Invariants and gotchas

- One `Tty` and one termios serve both ends; the master has no line discipline of its own, and
  "is the caller's ctty" is true for either end.
- Hangup is never done inside `Drop`; anything that closes the last master must leave the
  terminal on `CLOSED` (or call `hang_up` directly, as console close does).
- `/dev/tty` opens count as slaves, so they affect `slave_closed` and console end-of-session.
- `session` and `fg` are bare ids, not references.
- The output limit is soft: room is measured before `ONLCR` expansion and echoes bypass it.
- A deferred hangup whose terminal has no remaining references is skipped; nothing is left to
  signal, and `new_pty` prunes the dead table entry later.

## Confirmed bugs / technical debt

- **`TIOCSTI` lets anyone type into a terminal:** it checks neither that the terminal is the
  caller's controlling terminal nor `CAP_SYS_ADMIN`, so any process holding an fd to a pty
  (inherited, passed over a socket, kept after `setsid` or util-linux `su -c`) can inject commands
  into the shell reading it; on a master fd it injects into the slave instead of the master's own
  queue (Linux 6.1 returns `EPERM`).
- **`TIOCSCTTY` steals terminals without privilege:** argument 1 takes a terminal from another
  session with no `CAP_SYS_ADMIN` check, and neither it nor acquisition on open requires read
  access; a process that calls `setsid` and holds even a write-only fd (group tty has write access
  on console terminals) becomes the terminal's session and foreground, then reads the victim's
  input through `/dev/tty` and receives its `^C`/`^Z`.
- **tmux's pane queries fail:** `TIOCGPGRP`/`TIOCGSID` on a master fd return `ENOTTY` unless the
  pty is the caller's controlling terminal (a tmux server has none), breaking automatic window
  names and `pane_current_command`/`pane_current_path`; `TIOCNOTTY` is accepted on a master where
  Linux returns `ENOTTY`.
- **Echo can grow output without limit:** echoes and `send_xchar` ignore the output limit, so a
  master that never reads while the slave reads input lets the output queue grow unbounded
  (Linux drops echoes when full); one write can also overshoot by `ONLCR` expansion.
- **Non-canonical reads differ:** `VTIME` with `VMIN>0` is a total instead of an inter-byte timer;
  slave `POLLIN` ignores `VMIN`, and a nonblocking read with fewer than `VMIN` bytes queued returns
  `EAGAIN`, so level-triggered loops spin; a zero-length master write (or a blocked writer
  retrying) produces EPOLLET edges with no data.
- **Switching to canonical mode hides pending input:** raw data becomes the editing line and is
  unreadable until a line end (Linux pushes it as a readable line); `lnext` survives the switch.
- **`EXTPROC` does not disable local editing**, unlike Linux.
- **Background jobs are not stopped for some ioctls:** `TCFLSH`, `TCXONC`, `TCSBRK`/`TCSBRKP` and
  `TIOCSETD` skip the background check, so `tcflush`, `tcflow` and `tcdrain` from a background job
  get no `SIGTTOU`.
- **`TIOCSPGRP` error codes:** `EPERM` instead of `ESRCH` for unknown groups, `ENOTTY` instead of
  `EINVAL` for negative values from a non-ctty caller, `EIO` instead of `ENOTTY` for orphaned
  callers.
- **Leader exit sends an extra `SIGCONT`:** a session leader's exit with a PTY ctty sends `SIGHUP`
  and `SIGCONT` to the foreground group; Linux sends only `SIGHUP` there.
- **Console input after the session ended is queued silently** instead of failing with `EIO` as
  `write_console`'s comment promises.
- **Fidelity gaps:** `IUCLC`, `IMAXBEL`, `ECHOPRT`, `PARMRK`, break handling, `HUPCL`,
  `ONOCR`/`ONLRET`/`OLCUC`, tab expansion and `VDISCARD` are not implemented; erase echo differs
  (without `ECHOE` the erased character is echoed; tabs erase one column; `VWERASE` splits on
  whitespace); `VREPRINT` and `ECHOK` emit a newline even with `ECHO` off; `TIOCEXCL` is a no-op;
  `TIOCGETD` is always 0.

## Open questions

- Is mapping `/dev/console` to the caller's controlling terminal (rather than a system console)
  intended?
- Should EXTPROC suppress local editing as in Linux?

## To verify

- Linux 6.1's `TIOCOUTQ` and `TIOCSBRK`/`TIOCCBRK` results on ptys (believed 0 and success; here
  the output queue length and `ENOTTY`).
- The comparisons with Linux in this map come from knowledge of `n_tty.c`, `tty_io.c`,
  `tty_jobctrl.c` and `pty.c`, not from a source checkout; the differential test `pty` covers the
  common paths natively.
