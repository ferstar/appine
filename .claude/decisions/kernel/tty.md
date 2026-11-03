# Terminals (kernel/tty) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/tty"`
>
> Last distilled: 2026-11-03 (HEAD = c5236a1) — seeded by hand from the retired design doc §9 and
> the round records in `docs/rounds/`; no `MODULE: kernel/tty` Decision blocks exist yet, so
> sources cite rounds instead of commits.

## Active

### D1: PTYs and the line discipline live in the kernel layer, not on a host PTY
- **What**: Every PTY pair, its termios and line discipline, and job control are implemented in process; the app's terminal tabs and `appine-run` talk to kernel-held masters.
- **Why**: With a host PTY, the host's `ISIG` would send signals to host process groups, which know nothing about guest process groups; guest programs (tmux, screen, `script`, sshd) also need to allocate their own PTYs through `/dev/ptmx`.
- **Tradeoffs**: Every line-discipline feature has to be reimplemented; a subset of termios flags is supported.
- **Watch out**: The defaults must stay those of Linux's pts `init_termios`.
- **Source**: design review 2026-10-24 (§9), round 05 (`05-m3b`)

### D2: Closing the last master hangs the terminal up, deferred to the BKL release
- **What**: As in Linux's `pty_close` → `tty_vhangup`, the session leader gets SIGHUP and SIGCONT, the session loses its controlling terminal, and open slave ends read 0 and write `EIO`; the work runs when the closing thread releases the BKL.
- **Why**: Only decrementing a count left programs running after their terminal was gone (opencode kept redrawing after `tmux kill-server` and held memory until the OOM killer ended the next CLI). `Drop` cannot reach the kernel, hence the deferral.
- **Tradeoffs**: A short window exists where the master is gone but the hangup has not run (slave I/O returns `EIO` then).
- **Watch out**: Every path that closes a master must leave the terminal for `hang_up_closed` or hang it up itself.
- **Source**: round 104 (`104-pty-hangup`)

### D3: `vhangup(2)` and `TIOCVHANGUP` hang up one side by generation
- **What**: Each side keeps a generation; bumping it hangs up every end opened before, while the node stays reopenable.
- **Why**: util-linux `login` calls vhangup to evict processes left on a terminal, and agetty `--hangup` and systemd use `TIOCVHANGUP`; LTP's vhangup tests were skipped.
- **Tradeoffs**: Only users running `login`/`agetty` themselves benefit; the app logs in without them.
- **Watch out**: Permission differs per call (`CAP_SYS_TTY_CONFIG` vs `CAP_SYS_ADMIN`), as in Linux.
- **Source**: round 130 (`130-vhangup`)

### D4: termios2 ioctls are supported, with speeds recorded only
- **What**: `TCGETS2` and the `TCSETS*2` family share the kernel termios2 layout; baud rates are stored and reported but do nothing.
- **Why**: glibc ≥ 2.42 implements `tcgetattr` (and so `isatty`) only with `TCGETS2` and does not fall back, so on Ubuntu 26.04 every terminal was "not a tty" and readline stayed off.
- **Tradeoffs**: None in practice; a PTY has no line speed.
- **Watch out**: Keep `TCGETS` and `TCGETS2` views consistent (speed bits recomputed from CBAUD).
- **Source**: round 145 (`145-termios2`)

### D5: Packet mode (`TIOCPKT`) is implemented
- **What**: Master reads carry a status byte (flush, stop/start, DOSTOP/NOSTOP, IOCTL) or `TIOCPKT_DATA` before data.
- **Why**: GNU screen sets packet mode on every window's master and refuses to open the window when it fails; telnetd and rlogind use it too.
- **Tradeoffs**: `EXTPROC` only reports; local editing is not disabled.
- **Watch out**: A pending status byte must be returned alone and wake pollers with `POLLPRI`.
- **Source**: round 146 (`146-pty-packet-mode`)

### D6: A new PTY starts like Linux's: blocking master, 0×0 window
- **What**: Before a slave has ever been opened, master reads block; `EIO` and `POLLHUP` only follow once all opened slaves closed. New terminals start at 0×0.
- **Why**: screen reads the master right after forking the window's child, before the child opens the slave, and treated the early `EIO` as the window ending; `script` with a non-terminal stdin expects 0×0 as on native Linux.
- **Tradeoffs**: none
- **Watch out**: Console terminals take their size from the host instead.
- **Source**: round 150 (`150-new-pty-state`)

### D7: Flow control and flushes follow Linux's directions
- **What**: `TCFLSH` on the master flushes the right queue, and `TCOOFF` stops slave output so slave writes block.
- **Why**: The reversed direction and non-blocking TCOOFF made CPython's test_termios and test_ioctl hang until their 20-minute timeouts.
- **Tradeoffs**: none
- **Watch out**: Output stopped by `TCOOFF` cannot be restarted by `^Q`.
- **Source**: round 75 (`75-pty-flow`)

### D8: The host console mimics `script` at end of input
- **What**: Each app tab and each `appine-run` session is a console with a new session leader; when the host's input ends, the bridge waits until the program has read what was sent and then sends `VEOF` instead of hanging up.
- **Why**: `printf 'make\n' | appine-run --pty sh -i` left the shell waiting for input forever; util-linux `script` behaves this way, and tests drive interactive shells through pipes.
- **Tradeoffs**: Programs that ignore EOF keep running until the first process exits.
- **Watch out**: Resend `VEOF` if the program left canonical mode before reading it.
- **Source**: rounds 05 (`05-m3b`), 102 (`102-snmp-pty-eof`)
