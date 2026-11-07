# Synthetic filesystems (kernel/procfs) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/procfs"`
>
> Last distilled: 2026-11-07 (HEAD = 2d78930). D1–D8 were seeded by hand from the retired design
> doc §8.2 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: Synthetic files are generated from kernel state when opened, like seq_file
- **What**: Nothing is stored; opening a procfs, sysfs or cgroup file generates its whole content, later reads use that snapshot, and a read from offset 0 generates it again.
- **Why**: Linux's seq_file gives the same effect for a file read in one go, and generating from live kernel structures keeps every file consistent with the kernel layer without bookkeeping.
- **Tradeoffs**: Large files are generated in full per open; files that change do not notify pollers (mountinfo, `cgroup.events`).
- **Watch out**: Regeneration uses the current reader's view, not the opener's credentials.
- **Source**: design review 2026-10-24 (§8.2)

### D2: procfs and devpts mounts are new instances; sysfs and cgroup2 have one
- **What**: Every procfs mount gets its own device (and the mounter's pid namespace), every devpts mount its own set of terminals and options; sysfs and cgroup2 always share the boot instance.
- **Why**: Measured on native Linux 6.8: procfs (since 5.8) and devpts (since 4.7) are per mount, sysfs and cgroup2 single per namespace. Sharing the boot terminals let `ls /dev/pts` inside a container see the host's terminals, an isolation hole.
- **Tradeoffs**: procfs mount options (`hidepid`, `subset`) are still ignored.
- **Watch out**: ttyname compares device numbers between the fd and the `/proc/self/fd` path, so a terminal must report its own instance's device consistently.
- **Source**: rounds 105 (`105-mount-subset`), 167 (`167-devpts-instances`)

### D3: Memory and CPU figures come from the host, like a docker container
- **What**: `/proc/meminfo` and the memory part of `sysinfo` copy the host's values; the guest's real limit is exposed as cgroup `memory.max`. The guest sees the host process's CPUs, with uniform synthesized topology and host `cache/`/`cpufreq/` files.
- **Why**: Synthesized memory figures made `free` show 650 MB available instead of 3.4 GB and htop's bar look full; runtimes (JVM's container support, Node.js via libuv) already size heaps from cgroup limits. `nproc --all`, lscpu and glibc's `get_nprocs` count the sysfs CPU directories, which used to be missing.
- **Tradeoffs**: Programs that size themselves from `/proc/meminfo` alone see more memory than the OOM killer allows.
- **Watch out**: Keep `/proc/cpuinfo`, sysfs, `/proc/stat` and `sched_getaffinity` on the same CPU count.
- **Source**: rounds 39 (`39-loadavg-sysfs`), 46 (`46-mremap-sysfs`), 58 (`58-cgroup`), 68 (`68-meminfo`)

### D4: The root cgroup looks like a docker container's root
- **What**: `/proc/<pid>/cgroup` paths are relative to the reader's cgroup-namespace root, the root offers cpuset, cpu, memory and pids, and only `cgroup.procs`, `cgroup.threads` and `cgroup.subtree_control` are writable there; `memory.max`/`memory.current` at the root are the OOM killer's limit and measurement.
- **Why**: JVMs and Node.js recognise cgroup v2 and size heaps from it ("No metrics available" before); dockerd, podman, runc, crun and systemd then needed a real hierarchy with migration, built after Linux 6.1's `kernel/cgroup/cgroup.c`.
- **Tradeoffs**: No threaded cgroups, no chmod/chown delegation, no common-ancestor permission check on migration, no change notifications.
- **Watch out**: Mounts of cgroup2 are rooted at the caller's cgroup-namespace root.
- **Source**: rounds 58 (`58-cgroup`), 159 (`159-cgroup2-rw-mount`), 163 (`163-cgroup-tree`)

### D5: Only pids, memory, freeze, kill and the device filter are enforced
- **What**: `pids.max` stops fork and thread creation, `memory.max` is enforced by the OOM killer's periodic check (and immediately when lowered), `cgroup.freeze` stops threads transparently, `cgroup.kill` kills the subtree; other limits are format-checked and stored.
- **Why**: These are what `docker run --pids-limit`/`--memory`, podman's equivalents, `docker pause` and systemd's `TasksMax=`/`MemoryMax=` write. Host threads' scheduling is not under Appine's control, so `cpu.max`, `cpu.weight` and `cpuset.cpus` have nothing to act on.
- **Tradeoffs**: `memory.max` can be exceeded by tens of MB before the check notices; `memory.current` counts shared pages once per process; `memory.high`/`low`/`min` and swap limits do nothing.
- **Watch out**: Freezing must not look like a stop signal: parents and wait must not see it, and interrupted syscalls restart after thaw.
- **Source**: rounds 164 (`164-cgroup-limits`), 165 (`165-cgroup-freeze`)

### D6: bpf(2) supports only cgroup device programs, verified and interpreted
- **What**: Program load, attach, detach, query, get-fd-by-id and info for `BPF_PROG_TYPE_CGROUP_DEVICE`, with a verifier and error codes after Linux 6.1 and an interpreter; no maps, links or other program types.
- **Why**: cgroup v2 has no `devices.allow`; runc and crun attach a device program to every container cgroup and cannot skip it when the rules contain denials, so docker stopped at `bpf_prog_query` and podman at `bpf create`.
- **Tradeoffs**: Every other eBPF use (tracing, networking, maps) is unavailable.
- **Watch out**: Feature probes by cilium/ebpf fail gracefully (no map → no program names; no CgroupSKB → no `BPF_F_REPLACE`).
- **Source**: round 166 (`166-bpf-devices`)

### D7: devpts terminals keep their own owner, mode and times
- **What**: A terminal's owner, group, mode and times live on the terminal and can be changed with chown, chmod and utimensat.
- **Why**: sshd hands the terminal to the logged-in user and the tty group (`pty_setowner`); `chown` failing closed `ssh -t` and interactive logins, and with them mosh.
- **Tradeoffs**: The values disappear with the terminal (as on Linux).
- **Watch out**: Defaults come from the instance's mount options.
- **Source**: rounds 64 (`64-pty-owner`), 167 (`167-devpts-instances`)

### D8: Network files live under `/proc/<pid>/net`, per network namespace
- **What**: `/proc/net` is a link to `self/net`; each `/proc/<pid>/net` shows the target's network namespace.
- **Why**: Programs reading `/proc/self/net/...` or another process's net files got `ENOENT`, `ls -l /proc` and fd links differed from Linux, and network namespaces need per-namespace views.
- **Tradeoffs**: Counters and routes are real only in the initial namespace.
- **Watch out**: `/proc/sys/net` follows the reader's namespace, `/proc/<pid>/net` the target's.
- **Source**: round 96 (`96-proc-pid-net`)

### D9: procfs, sysfs, devpts, cgroup2 and the bpf device subset are one module
- **What**: kernel/procfs covers `pseudo.rs`, `cgroup.rs`, `bpf.rs` and `syscall/bpf.rs`, keeping cgroup2 and the bpf device subset here rather than in a cgroup module of their own.
- **Why**: The retired design doc's §8.2 gave a few lines to about 6.7k lines of procfs, sysfs, devpts, cgroup and bpf code.
- **Tradeoffs**: The name understates the cgroup and bpf content, and the boundaries with kernel/mm, kernel/process, kernel/signal, kernel/net and kernel/tty are kept by reference.
- **Watch out**: Bugs confirmed at the split or found by Starnix's tests stay open in the map, among them `CLONE_INTO_CGROUP` without a permission check, `cgroup.procs` migration without the common-ancestor check, no poll notifications, `/proc/<pid>/fd` open to everyone, `/proc/sys` permissions following plain DAC and no zoneinfo, pressure or kallsyms files.
- **Source**: c5236a1 (bug list since updated by 173da76 and 4e64e74)

### D10: `/proc/stat`'s btime is computed in nanoseconds on every read and follows the time namespace
- **What**: Each read takes realtime in nanoseconds minus `with_boot_offset(uptime_ns())`, then whole seconds (no less than 0), so btime stays fixed and moves back by the reader's boottime offset.
- **Why**: Subtracting two clocks already truncated to seconds made btime alternate between two values, so `ps -o lstart` (procps-ng: btime plus start time / HZ) changed between reads, and the time-namespace offset Linux 6.1 subtracts (`timens_sub_boottime`) was ignored.
- **Tradeoffs**: The two clocks are read microseconds apart, so btime can still be one second off when the boot instant's fraction falls in that gap; unlike a cached value it follows host clock steps, as Linux does.
- **Watch out**: A host clock step (Android network time) moves btime and programs that cached it disagree until they reread it; the difftest cases (compat2, containers) compare only stability and offsets, not exact values.
- **Source**: eb3bc8b

### D11: smaps' VmFlags show `dc` and `wf`
- **What**: VmFlags lists `dc` (dontfork) and `wf` (wipeonfork) in Linux's bit order.
- **Why**: smaps had no way to show the fork advice flags that kernel/mm D19 added.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: e020977

### D12: Any thread id resolves under `/proc` through its thread group
- **What**: `group_of` finds a pid node's thread group by group id or by the task's tgid and every pid-node lookup uses it, so `/proc/<tid>` (not listed by readdir) shows the thread's status, comm, cwd and fd with its group's Tgid while stat adds up the group, as Linux's tgid view of a thread does.
- **Why**: Thread-aware programs open `/proc/<gettid()>/comm`, stat or status directly and got `ENOENT`.
- **Tradeoffs**: A `/proc/<tid>` inode mixes in its group's serial, so a thread id reused within the same process keeps its inode number.
- **Watch out**: Code outside procfs that treats a pid node's id as a thread group id can still miss threads; the known ones (exe, pidfd_send_signal's directory) use `group_of`.
- **Source**: 173da76

### D13: Synthetic filesystem files read in the order their code runs
- **What**: pseudo.rs runs lookup, stat and readdir, per-process files, system-wide files, /proc/<pid>/net, sysfs, writing, contents and shared helpers; cgroup.rs goes from mkdir through reading, writing, fork, the OOM killer and device programs to members; bpf.rs runs programs, verifier, interpreter; syscall/bpf.rs follows dispatch order; syscall/mqueue.rs, filed here, puts the calls taking a name before those taking an fd.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; pseudo.rs had 57 functions above their first caller.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; `write_mem` stays with `read_mem`, and `file_mode` with `FILES` since stat calls it.
- **Watch out**: Code appended at the end of a file drifts from this order; syscall/mqueue.rs has since been registered under kernel/ipc.
- **Source**: a63a7b4; registry since 4140c75

### D14: Loop devices appear under `/sys/block`
- **What**: Each loop device has a node under a new `/sys/block/` with dev, removable, ro, size, `queue/*` and, while bound, `loop/*`, read from `Loops::sysfs_read`, as plain directories like `/sys/class/net`.
- **Why**: LTP's ioctl_loop tests check loop state there, and statx11's setup loops forever without `/sys/block/<dev>/queue`.
- **Tradeoffs**: There is no `/sys/dev/block` and no uevents, so ioctl_loop05 and udev-style lookups still fail.
- **Watch out**: Tools expecting `/sys/block` entries to be symlinks into `/sys/devices` can misbehave.
- **Source**: a26c8b4

### D15: Memory views follow Linux's stack and file-name rules
- **What**: `/proc/sys/vm/mmap_min_addr` is a global sysctl (writes need `CAP_SYS_RAWIO`), VmStk comes from grows-down VMAs, newlines in mapped file paths print as `\012`, and `/proc/<pid>/mem` grows stacks within the accessor's `RLIMIT_STACK`.
- **Why**: mmap_min_addr had no sysctl, VmStk read 0 and maps printed newlines in file names raw.
- **Tradeoffs**: VmData, VmExe and VmLib stay approximate.
- **Watch out**: None known.
- **Source**: e03c0cc

### D16: Common sysctls exist, and root can write the read-only ones
- **What**: 40 recorded sysctls with 6.1's ranges are kept in `Kernel::sysctls`, with kernel.modprobe, kernel.tainted and vm.overcommit_memory (kernel/mm D24) handled on their own and kptr_restrict, drop_caches and mmap_rnd_bits given Linux's permissions.
- **Why**: Setup scripts write these sysctls and checkers read them, and `sysctl -p` and `sysctl -w` failed where Linux succeeds because the files were missing or read-only for root.
- **Tradeoffs**: The recorded values change nothing, and randomize_va_space starts at 0, true of Appine's fixed layout, where Linux starts at 2.
- **Watch out**: Programs that read a recorded value back may act as if the setting applied.
- **Source**: 42d2ffe

### D17: An exited main thread shows as a zombie until the process is reaped
- **What**: The process directory no longer falls back to the first live thread: listing, link and thread counts, state (Z for the exited leader), comm and CPU time use `ThreadGroup::leader_exit` (kernel/process D21), exe is `ENOENT` without a live task, and `/proc/<tid>`'s state follows that thread.
- **Why**: Linux keeps an exited main thread as a zombie until the group is reaped, while Appine dropped it and let the process directory describe another thread, which alone failed 35 Starnix tests.
- **Tradeoffs**: Entries that need a task (cmdline, maps, fd/, the mount namespace) behave as for a zombie while the rest of the process runs, as on Linux.
- **Watch out**: Code that reached a live thread's state through the process directory after the leader exited gets nothing (none was found in the kernel).
- **Source**: 79a60cf

### D18: `/proc/vmstat` is the host's
- **What**: `/proc/vmstat` copies the host's file read through hal (D13 there), or a 16-line fallback when it cannot be read.
- **Why**: procps' vmstat cannot start without it, and meminfo already shows the host's memory figures (D3).
- **Tradeoffs**: The counters are the whole device's, not the guest's, and the fallback's event counters are 0.
- **Watch out**: Android may refuse `/proc/vmstat` to apps, so phones are likely to show the fallback.
- **Source**: 60921b2

### D19: Thread names are shown as their bytes
- **What**: `comm_of` returns bytes, and comm, stat and status are built as byte vectors, with status escaping the name as Linux does.
- **Why**: Linux prints the raw bytes of a thread name, which Rust strings cannot hold when they are not UTF-8.
- **Tradeoffs**: Log lines elsewhere (OOM kills, bug reports, the kernel log) still show the name lossily.
- **Watch out**: None known.
- **Source**: 6591eb6

### D20: status splits VmRSS into RssAnon, RssFile and RssShmem
- **What**: status prints the three lines from kernel/mm's `resident_split` (D26 there) and VmRSS as their sum.
- **Why**: Linux's status has these lines, and VmRSS is their sum.
- **Tradeoffs**: statm and stat still use `resident_pages`, which can differ slightly from status.
- **Watch out**: None known.
- **Source**: 566da95
