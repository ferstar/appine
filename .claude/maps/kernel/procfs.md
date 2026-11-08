# kernel/procfs Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/procfs.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 5 research concerns; 21 claims checked by 3 independent verifiers — 14 confirmed and 7 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-08 (targeted verification only since the full mapping: `/proc/net` owners; `/proc/<tid>`; `/sys/block`; mm fields; names; test-suite findings; fd access, link modes; cgroups; write opens; block sizes, affinity, bpf rights; `/proc/sys` permissions and IDs; `softirq` line; overlay parameters; exception-trace; oom_score, swaps, locks, task oom files last)

## Responsibilities

`kernel/procfs` provides the synthetic filesystems, generated live from kernel state with nothing
stored:

- procfs: the per-process and per-thread trees, system-wide files, `/proc/sys` (sysctls, many
  scoped by namespace) and `/proc/<pid>/net`;
- sysfs: CPU topology (partly copied from the host), `/sys/class/net`, `/sys/block` (loop
  devices), overlayfs's module parameters, the cgroup mount point;
- devpts as a filesystem (instances, nodes, owners and modes) and nsfs nodes;
- the node views of the mqueue filesystem;
- cgroup v2: the cgroup tree, its interface files, membership and migration, and the limits that
  are enforced (pids, memory via the OOM killer, freeze, kill, depth/descendants);
- a bpf(2) subset that verifies and interprets cgroup device programs.

Root: `core/kernel/src/pseudo.rs`, `cgroup.rs`, `bpf.rs`, `syscall/bpf.rs`.

Outside the module: the `Pseudo` trait, the walker, mounts and mountinfo order, open/read/write
dispatch for pseudo nodes and magic-link resolution (`kernel/vfs`); maps, smaps, pagemap and
`/proc/<pid>/mem` semantics (`kernel/mm`, `kernel/signal`); id maps, `setgroups`,
`timens_offsets`, `ns/*`, `children`, state letters and namespaces (`kernel/process`);
`/proc/kmsg`, `/proc/version` and ABI sysctls (`kernel/signal`); terminal behaviour
(`kernel/tty`); network semantics behind `/proc/net` and `net.*` (`kernel/net`); the OOM killer
(`kernel/mm`); SysV IPC and message queues (`kernel/ipc`).

## Key types

- `PseudoNode` — every synthetic node: devpts (`PtsRoot(dev)`, `Pts(dev, n)`, `PtsPtmx(dev)`),
  procfs (`Global`, `SelfLink`, `ThreadSelf`, `Static`, `Pid`, `Entry`, `Net`, `Fd`, `OpenFd`,
  `OpenExe`, `FdInfo`, `Ns`, `NetConf`, `NetSysctl`), sysfs (`SysRoot`, `Sys`, `SysCpu`, `SysNet`,
  `SysBrif`, `SysBlock`), `Cgroup`, `NsFile(kind, id)`, mqueue nodes.
- `Global`/`GLOBALS` (15 top-level files), `Entry`/`ENTRIES` (33 per-process entries),
  `TASK_ENTRIES` (27 per-thread entries), `NsKind`, `STATIC` (90 fixed `/proc/sys` and
  `/proc/sysvipc` paths), `SYS_STATIC` and the sysfs file tables, `PROC_NET` (18 files).
- `View` — a read-only view of the kernel for one reader thread, taken under the BKL; implements
  `Pseudo`. `Offline` serves an empty root before boot.
- `FileKind::Proc` — an open synthetic file: mount, node, snapshot `data`, `fresh` flag, mm for
  pagemap/mem, namespace reference for nsfs, kmsg reader.
- `Cgroup` (name, parent, children, `subtree_control`, owner, written values, events, peak, freeze,
  attached device programs) and `Cgroups` (the tree; ids never reused, root 1); `CONTROLLERS`,
  `FILES` (39 interface files), `Writer`; `CgroupNs` (in nsproxy).
- `bpf::Prog`, `bpf::Registry` (weak, by id), `FileKind::Bpf`.
- `PtsOptions` (kept by the VFS per devpts instance) and the owner, mode and times kept on `Tty`.

## Public entry points

- `Pseudo for View`: `lookup`, `parent`, `name`, `kind`, `readlink`, `follow`, `follow_dentry`,
  `magic`, `protected_symlinks`.
- `pseudo::stat`, `node_ino`, `ino_of`, `fs_of`, `readdir`, `content`, `writable`, `unreadable`,
  `write`, `written`, `read_pagemap`, `read_mem`, `write_mem`, `describe`, `sockets`,
  `socket_ino`, `pty`.
- cgroup: `file`, `write`, `Cgroups::mkdir`, `rmdir`, `proc_cgroup`, `proc_cgroups`,
  `pids_charge`, `may_enter`, `memory_limits`, `measure_memory`, `memory_event`, `oom_group`,
  `effective_device_progs`, `hierarchy_allows_attach`, `file_mode`.
- bpf(2) (`syscall::bpf::bpf`) and `check_device`.

## Data flow / lifecycle

### Instances, lookup and metadata

- Each procfs mount is a new instance with its own device; if the mounter is in a non-initial pid
  namespace, the instance is bound to it, otherwise it uses the initial one. Mount options
  (`hidepid`, `subset`) are ignored.
- `/proc` readdir: `self`, `thread-self`, the top-level static directories (`sys/`, `sysvipc/`),
  the global files, then the processes visible in the instance's namespace, sorted. A pid name
  is mapped through that namespace and may name any thread (`group_of`), though readdir lists
  only processes: `/proc/<tid>` is its process's directory seen through that thread (`status`,
  `comm`, `cwd` and `fd/` are the thread's, `stat` adds up the group's CPU time, `task/` lists the
  group), and it disappears when the thread exits. `self`/`thread-self` are `ENOENT` for
  readers not visible in the instance. Numeric names (pids, fds) are decimal digits only, at most
  10 without a leading zero and at most `i32::MAX` (Linux's `name_to_int`): a sign or a larger
  number names nothing.
- Inode numbers hash the node (low bit set); roots are 1; nodes under a pid mix in the thread
  group's serial, so a reused pid gets new inodes; nsfs files use the namespace id. Inodes are
  stable within one run, not across runs.
- Modes: directories 0555 (0755 on sysfs and cgroup), `fd/<n>` links by the open mode as
  `proc_fd_instantiate` (0500 readable, 0300 writable, 0700 both, 0 for `O_PATH`), other symlinks
  0777, `mem` 0600, recorded sysctls Linux's (`vm.drop_caches` 0200, `vm.mmap_rnd_bits` 0600),
  other writable files 0644, `pagemap`, `auxv` and kmsg 0400, everything else 0444 (so `fd/`,
  `fdinfo/`, `ns/` are 0555,
  `environ` 0444, `comm` and `projid_map` read-only); cgroup files use their own table modes,
  `NetSysctl` files theirs, devpts nodes the terminal's mode and `ptmxmode`; `net.*` sysctls belong
  to root of the user namespace owning the network namespace when it maps one
  (`net_ctl_set_ownership`). Stat reports
  `st_blksize` 1024 for procfs and devpts nodes (their superblocks' block size), 4096 elsewhere. Owners: per-process
  entries (including the `fd/`, `fdinfo/`, `ns/`, `net/` directories) belong to the process's
  euid/egid, or the root of its credentials' user namespace when not dumpable; the entries inside
  `fdinfo/`, `ns/` and `net/`, reopened open files and everything else belong to root.
  Times are "now" except devpts terminals; directories have nlink 2 (`task/` 2 plus threads);
  sizes are 0 except sysfs and cgroup files (4096).

### Opening, reading and writing

- Open (in `kernel/vfs`'s pseudo open): DAC against the synthetic stat (`O_TRUNC` asks for write
  permission too); for `mem`, `auxv`, `environ`, `maps`, `smaps`, `smaps_rollup` and `pagemap` one
  ptrace check with filesystem credentials and no read/attach distinction (`EACCES`, as Linux's
  `proc_mem_open`), and the magic-link check (`may_read`) also uses filesystem credentials and
  gates `fd/` and `fdinfo/` (listing and lookup in them, and reading or following `fd/N`;
  `EACCES`), as Linux's `proc_fd_permission` and `proc_fd_access_allowed` do; writing `setgroups`
  needs `CAP_SYS_ADMIN` in the process's user namespace (`EACCES`, `proc_setgroups_open`); devpts
  nodes on nodev mounts `EACCES`, then the cgroup device filter; write modes on ro mounts
  `EROFS`; namespace files are immutable (any write mode or `O_TRUNC` is `EPERM`, before DAC);
  `/proc/sys` decides as `proc_sys_permission`/`test_perm` do, not by DAC: the owner bits for euid
  0, the group bits for a member of group 0, the others' otherwise, capabilities aside, and a
  `net.*` entry gives its owner bits to anyone with `CAP_NET_ADMIN` over the owner of the reader's
  network namespace (`net_ctl_permissions`, so root of a container writes its namespace's);
  `user/max_*` keep their own rule (`CAP_SYS_RESOURCE` in the user namespace). A
  node that is not writable still opens for writing when DAC passes (root) if it is a procfs file
  outside `/proc/sys`, and each write then fails: `EIO` for the files of proc entries (global files
  such as `cpuinfo` and `kmsg`, `/proc/sysvipc/*`, the single-record `netstat`, `snmp`,
  `sockstat` and `sockstat6` of `/proc/<pid>/net`; `proc_reg_write`), `EINVAL` for per-process and
  `fdinfo` files (no write operation). `/proc/sys` refuses it (`EACCES`, `O_TRUNC` included); sysfs
  (`kernfs_fop_open`) and the other `/proc/<pid>/net` files (`seq_open_net`) refuse only a write
  access mode, so `O_TRUNC` alone opens them; creating names `EACCES`.
- The content is generated once at open (`fresh`). The first read at offset 0 uses it; later reads
  or preads at offset 0 regenerate it with the opener's credentials (kept with the open file,
  Linux's `f_cred`) and the current reader's namespaces; reads elsewhere use the current snapshot; lseek (`SEEK_SET`/`CUR`/`END` against the
  snapshot length) never regenerates. `pagemap`, `mem` and `/proc/kmsg` are computed at read time;
  nsfs files cannot be read (`EINVAL`) and, like write-only cgroup files, never regenerate; a
  `NetConf` file whose generation fails at open reports the error on the first read.
- Writes pass the whole buffer of each call to `pseudo::write` with the writer's credentials and
  namespaces (the id maps, `setgroups` and `timens_offsets` also check the opener's, `kernel/process`),
  apply immediately, do not advance the position (except `mem`), and mark the snapshot stale.
  Per-process numbers use `proc_number`/`kstrto` (first 12 bytes, base 0, `EINVAL`/`ERANGE`);
  most sysctls parse decimal after trimming.
- Synthetic files (other than kmsg) cannot be added to epoll (`EPERM`) and poll as always ready, so
  mount table changes are never signalled through mountinfo.

### Per-process files

- Entries: `auxv`, `stat`, `status`, `statm`, `cmdline`, `comm`, `environ`, `limits`, `maps`,
  `mem`, `smaps`, `smaps_rollup`, `pagemap`, `mounts`, `mountinfo`, `cwd`, `exe`, `root`, `fd/`,
  `fdinfo/`, `task/`, `net/`, `oom_score`, `oom_score_adj`, `coredump_filter`, `cgroup`, `ns/`,
  `uid_map`, `gid_map`, `projid_map`, `setgroups`, `timens_offsets`; `task/<tid>` has the same
  minus `task`, `pagemap`, `coredump_filter`, `timens_offsets`, plus `children` (its `oom_score`
  and `oom_score_adj` are the process's, and writing the latter sets the process's, as Linux's
  tid_base_stuff). Absent: `oom_adj`, `io`, `wchan`, `sched*`, `personality`,
  `loginuid`/`sessionid`, `attr/`, `stack`, `syscall`, `cpuset`, `mountstats`, `timerslack_ns`.
- `oom_score` is the OOM killer's badness scaled against the host's RAM plus swap (`kernel/mm`'s
  `proc_score`): 0 to 2000, 0 for the built-in init, `oom_score_adj` -1000, a process without
  live threads and a vfork child its parent still waits for.
- `stat`: all 52 fields; fault counters, code/stack addresses, `wchan`, `processor`, priority and
  policy are 0; start time in clock ticks plus the reader's time-namespace boottime offset; the
  thread's or the group's CPU times depending on the view; `tty_nr` from the controlling terminal.
- `status`: ids translated into the reader's user namespace, `NStgid`/`NSpid`/`NSpgid`/`NSsid`
  from the instance's namespace down, capabilities, `NoNewPrivs`, `Seccomp`, signal masks; memory
  lines approximate (`VmPeak` = `VmSize` = `VmData`, `VmHWM` from max RSS, `VmRSS` the sum of
  `RssAnon`, `RssFile` and `RssShmem` from `Mm::resident_split`, `VmStk` the grows-down mappings,
  exe/lib/PTE/swap 0; omitted with no live task); `FDSize` rounded to 64;
  `Cpus_allowed_list` is the thread's affinity (the main thread's in the process's `status`)
  within the online CPUs. `maps` and `smaps` escape a newline in a mapped
  file's path as `\012` (Linux's `seq_file_path`).
  `statm` is `size rss 0 0 0 rss 0`.
- `cmdline`/`environ` copy the argument and environment ranges of the current mm (empty over
  1 MiB or when unreadable); `comm` is read-only; `limits` lists the rlimits. The thread name is
  bytes, not text: `comm` and `stat` give it as set, `status` escapes only a newline and a
  backslash (Linux's `proc_task_name`).
- `fd/` lists the fds of the group leader's table (also under `task/<tid>`, even for a thread with
  its own table); `fd/N` links show the
  path stored at open with ` (deleted)` appended when nlink is 0; `fdinfo/N` has `pos`, `flags`
  (octal, with `O_CLOEXEC`), `mnt_id`, `ino`, the `lock:` lines of the locks held through that
  open file (`kernel/vfs`'s `fd_locks`; only for files or fd tables that have taken locks) and
  per-kind lines (eventfd count, inotify watches, epoll `tfd:` lines, timerfd clock and times,
  signalfd mask, pidfd pids).
- `mounts`/`mountinfo` come from the VFS mount table for the process's namespace and root (an exited
  thread has none: `EINVAL` at open, as `mounts_open_common`); `cgroup` is the path relative to the reader's
  cgroup-namespace root; `net/` uses the target's network namespace.
- Writable: `oom_score_adj` (-1000..1000; going below the minimum needs `CAP_SYS_RESOURCE`, and a
  privileged write sets the new minimum), `coredump_filter` (masked to 0x1ff), `mem`, the id maps,
  `setgroups`, `timens_offsets`.
- Zombies keep their group without tasks: stat/status work (state Z), cmdline/environ/auxv are
  empty, `fd/` is empty, `cwd`/`root`/`exe`/`ns` are `ENOENT`, `mounts`/`mountinfo` are `EINVAL`
  to open and `mem`/`pagemap` `ESRCH` (they use the address space of the thread named, which has
  none; native 6.8 answers the same). A main thread that has exited while
  other threads run is shown the same way, as Linux's zombie leader: `task/` still lists it,
  `task/<pid>` and the process directory report Z with its name (kept in the group, as for
  zombies) and its own CPU time for `task/<pid>/stat`, and `Threads`/`num_threads` and `task/`'s
  link count include it; the process directory no longer falls back to another thread.

### System-wide files and `/proc/sys`

- `meminfo`: 16 fields copied from the host's `/proc/meminfo` (falling back to host `sysinfo`), not
  clamped to cgroup limits. `vmstat`: the host's file when the app may read it, else the page
  counts meminfo gives (free, slab, anon, mapped, file, dirty, shmem) and the event counters
  procps reads at 0. `sysinfo(2)` takes memory from the host but uptime (with the time
  namespace offset), loads and the process count from the guest.
- `cpuinfo`: one block per guest CPU with the guest-visible hwcaps and MIDR, BogoMIPS 100.
  `stat`: guest threads' CPU time (plus exited threads') with idle derived from uptime, split
  evenly over CPUs; `procs_running` counts threads in guest code (at least 1); `intr`, `ctxt`,
  `softirq` (the total and Linux's 10 types), `procs_blocked` are 0; `processes` counts every new task, threads included (Linux's `total_forks`). `uptime`
  includes the reader's
  boottime offset. `loadavg` comes from the guest's own load average. `filesystems` is a fixed
  list; `cmdline` is `appine`; `cgroups` from the cgroup tree. `mounts` and `net` link into
  `self`. `swaps` is the host's file when the app may read it (as meminfo's swap lines are the
  host's), otherwise Linux's header line alone; `locks` lists every lock and lease
  (`kernel/vfs`'s `proc_locks`) with pids in the instance's pid namespace.
- Absent: `partitions`, `devices`, `sys/fs/file-nr`, `protected_regular`/`protected_fifos`,
  Yama.
- Sysctl scoping: SysV IPC limits and `fs/mqueue/*` per the writer's IPC namespace; `hostname`/
  `domainname` per UTS namespace (one trailing newline stripped, over 64 bytes `EINVAL`);
  `ns_last_pid` per pid namespace; `net.*` per network namespace (`ip_forward` aliases
  `conf/all/forwarding`; `NetConf` directories per interface); `user/max_*_namespaces` per user
  namespace (the reader's or writer's: the limits of `kernel/process`'s namespace counts,
  `INT_MAX` by default in a new one, 0 to `INT_MAX`); everything else kernel-wide
  (`max_map_count`, `mmap_min_addr`, inotify limits, `aio-max-nr`, `lease-break-time`, `nr_open`,
  `mount-max` (1 to `INT_MAX`; the mount limit of kernel/vfs),
  `pipe-max-size`, `core_pattern`, `core_uses_pid`, `protected_symlinks`/`hardlinks`, `printk*`,
  `panic`; `dmesg_restrict`, `unprivileged_bpf_disabled` and seccomp
  `actions_logged` need `CAP_SYS_ADMIN` in the initial namespace, `mmap_min_addr`
  `CAP_SYS_RAWIO`).
- Several writable sysctls are recorded without effect (`kernel.panic`, every
  `NetSysctl` entry, `ping_group_range`, `somaxconn`, `ip_local_port_range`; of the port sysctls
  only `ip_unprivileged_port_start` is used, by bind); `ping_group_range` keeps kernel group IDs,
  shown in the reader's user namespace (65534 when unmapped) and written from the writer's (an
  unmapped one is `EINVAL`); `kernel/pid_max` accepts writes and ignores
  them. The `RECORDED` table adds 40 more with Linux 6.1's modes, defaults and accepted ranges
  (`overflowuid`/`gid`, `vm.dirty_*`, `drop_caches`, `swappiness`, `overcommit_ratio`,
  `page-cluster`, `watermark_scale_factor`, `mmap_rnd_*bits`, `kernel.sysrq`,
  `randomize_va_space` (default 0: nothing is randomized), `kptr_restrict` (writes need
  `CAP_SYS_ADMIN`), `perf_*`, `hung_task_*`, `sched_rt_*`, `sched_schedstats`, `panic_on_oops`,
  `core_pipe_limit`, `modules_disabled` (only 1 can be written), `fs.suid_dumpable`,
  `debug.exception-trace`), kept kernel-wide in `Kernel::sysctls` and read through
  `recorded_value`; all change nothing except `debug.exception-trace` (default 0, arm64's
  `show_unhandled_signals`, or 1 when the host asks through `MachineOptions::exception_trace`, as
  the app does), which turns on kernel/signal's unhandled-exception reports when non-zero; `drop_caches` is write-only (reads `EINVAL`). `kernel.modprobe` keeps a string, `kernel.tainted` only gains flags (of the 19
  defined; writes need `CAP_SYS_ADMIN`), and `vm.overcommit_memory` (0 to 2) is used: 1 turns off
  fork's commit check (`kernel/mm`). Constants: `file-max`, `cap_last_cap` 40, `ngroups_max`,
  `pid_max` 4194304; `random/uuid` is fresh per read, `boot_id` fixed per boot, the entropy values
  256.
- Write permission is plain DAC against root-owned files (0644 when writable) with no
  per-namespace grants, so `CAP_DAC_OVERRIDE` counts only where root is mapped in the writer's
  user namespace; except `user/max_*`, which anyone may read and holders of `CAP_SYS_RESOURCE` in
  their user namespace may write, whoever owns the files (Linux's `set_permissions` for the
  per-namespace tables).
- `/proc/<pid>/net/*` (`arp`, `dev`, `icmp`, `icmp6`, `if_inet6`, `ipv6_route`, `netstat`, `raw`,
  `raw6`, `route`, `snmp`, `sockstat`, `sockstat6`, `tcp`, `tcp6`, `udp`, `udp6`, `unix`) uses the
  target task's network namespace: sockets from every fd table plus in-flight AF_UNIX sockets,
  filtered by namespace, uids mapped for the reader; host interface counters and routes only in
  the initial namespace (new namespaces show zeros); `arp` is a header line.

### sysfs

- Tree: `block/`, `class/net/`, `devices/system/cpu/` (`online`, `possible`, `present`, `offline`, one
  `cpu<n>/` per guest CPU with synthesized `topology/` — each CPU its own core, one package — and
  `regs/identification/`), `cache/` and `cpufreq/` files copied from the host when the host lets
  the app read them (CPU numbers translated, missing files absent), the empty `fs/cgroup/`
  mount point, and `module/overlay/parameters/` with 6.1's Kconfig defaults, which are what the
  overlay does (`redirect_always_follow` Y, `redirect_max` 256, `check_copy_up`, `index`,
  `metacopy`, `nfs_export`, `redirect_dir`, `xino_auto` N; containerd adds `index=off` and
  BuildKit `redirect_dir=off` because the files exist). No `kernel/`, other modules, `dev/`,
  `bus/`, `firmware/`, `power/`, NUMA nodes, other classes or `uevent` files.
- `/sys/block/loop<n>/` exists for every loop device (`kernel/vfs`): `dev`, `removable`, `ro`,
  `size` (sectors), `queue/` (logical, physical, minimum and hardware sector sizes, optimal I/O 0,
  `dma_alignment` 511, `rotational`, `read_ahead_kb`, `max_sectors_kb`, `rq_affinity`), and while
  the device is bound `loop/` (`backing_file` as the backing open file's display path, `offset`,
  `sizelimit`, `autoclear`, `partscan`, `dio`). The values come from `Loops::sysfs_read`; device
  directories are directories rather than symlinks into `/sys/devices/virtual/block`.
- The guest CPU count is a snapshot of the host process's affinity at kernel creation (capped at
  64); the same count drives sysfs, `/proc/cpuinfo`, `/proc/stat`, `status`, `sched_getaffinity`
  and cgroup `cpuset`; guest CPU i is host CPU `host_cpus()[i]`.
- `/sys/class/net/<if>/` follows the reader thread's network namespace; interfaces are directories
  (Linux uses symlinks into `/sys/devices`); bridges have `bridge/` and `brif/` (symlinks to the
  ports' `brport/`), ports `brport/`; only byte and packet counters of host interfaces are real;
  `carrier` of a down interface is `EINVAL`, a removed interface `ENODEV`.
- sysfs is read-only (write opens `EACCES`, chmod/chown `EACCES`); one instance, so new mounts
  share the boot device and stacking on its root is `EBUSY`.

### devpts and nsfs

- Each devpts mount is an instance (`PtsRoot(dev)`) with its own terminals and `PtsOptions`
  (`uid`, `gid`, `mode`, `ptmxmode`, `max`; defaults `mode=600,ptmxmode=000`; the boot instance
  `gid=5,mode=620,ptmxmode=666`; `newinstance` accepted and ignored; unknown options or unmapped
  ids `EINVAL`).
- Opening `ptmx` allocates the lowest free index (`ENOSPC` at `max`); the terminal's owner is the
  option's uid/gid or the opener's fsuid/fsgid, its mode the option's mode; chmod, chown and
  utimensat are stored on the terminal. Nodes exist while the terminal does; readdir lists `ptmx`
  and every live terminal. Inodes: root 1, `ptmx` 2, terminal n is n+3; devices 136:n and 5:2;
  the root is 0555 (Linux 0755). fstat on a master fd always reports inode 2, mode 0666, rdev 5:2
  on the root filesystem's device, whatever the instance and `ptmxmode`.
- nsfs: following `/proc/<pid>/ns/*` (after the ptrace-read check) yields a regular 0444 file
  whose inode is the namespace id, device 0:4, statfs `NSFS_MAGIC`; opening keeps the namespace
  alive; reading is `EINVAL`; bind-mounting it pins the namespace.

### cgroup v2

- One global tree; the root (id 1, owned by root) looks like a docker container's root under a
  cgroup namespace with cpuset, cpu, memory and pids available. cgroup2 is mounted read-write at
  boot on sysfs's `fs/cgroup`; any mount is rooted at the caller's cgroup-namespace root
  (mountinfo shows `nsdelegate,memory_recursiveprot`).
- mkdir: `EROFS` on ro mounts; `EEXIST` for `.`, `..`, existing names and interface-file names;
  write and search on the parent; newline in the name `EINVAL`; ancestors' `cgroup.max.depth` /
  `max.descendants` `EAGAIN`; the new cgroup and its files belong to the creator's fsuid/fsgid,
  which is the only way to own one (path chown/chmod `EACCES`, or `EROFS` on ro mounts; fd-based
  `EBADF`). The root is 0:0 0755, so only privileged processes create top-level cgroups.
  rmdir: write and search on the parent; children or live processes `EBUSY`; the root `EBUSY`.
- Membership: every process starts in the root; fork places the child in the parent's cgroup or a
  `clone3(CLONE_INTO_CGROUP)` target after `pids_charge` (`EAGAIN`) and starts it frozen if the
  target is freezing; the target has to exist (`ENODEV`), the caller must be allowed to write its
  `cgroup.procs` and that of the nearest cgroup above both it and the parent's (`EACCES`, judged
  by mode and owner as an open would be), both must lie inside the caller's cgroup-namespace root
  (`ENOENT`), and the target must have no controllers enabled for children (`EBUSY`). Zombies stay
  until reaped; `CLONE_NEWCGROUP`/unshare
  roots the new namespace at the current cgroup. The BKL is held from the charge until the child
  is registered, so no fork escapes a concurrent `cgroup.kill`.
- `cgroup.procs` writes: pid 0 is the caller; pids resolve in the writer's pid namespace
  (`ESRCH`); kernel init `EINVAL`; the opener's credentials must allow writing the `cgroup.procs`
  of the nearest cgroup above both source and destination (`EACCES`, Linux's
  `cgroup_procs_write_permission`; the destination's own was checked at open); source or
  destination outside the writer's cgroup-namespace root `ENOENT`; a non-root destination with
  controllers enabled for children `EBUSY`; then the group moves and freezing is re-evaluated.
  Reads of
  `cgroup.procs`/`cgroup.threads` print ids in the reader's pid namespace, 0 for ones it cannot see
  (Linux's task_pid_vnr). Any write to a file of a removed cgroup is `ENODEV`. `cgroup.threads` only accepts
  moves within the same cgroup (`EOPNOTSUPP` otherwise); `cgroup.type` is always `domain`.
- `cgroup.subtree_control`: `+name`/`-name` tokens (`EINVAL`); enabling a controller not in
  `cgroup.controllers` `ENOENT`; enabling while the cgroup has processes `EBUSY` (except at the
  real root); disabling one a child still delegates `EBUSY`; disabling clears the children's stored
  values for it.
- On the root and on the writer's namespace root only `cgroup.procs`, `cgroup.threads` and
  `cgroup.subtree_control` are writable (`EPERM` otherwise). Writing a read-only file is `EINVAL`.
- Enforced: `pids.max` (fork and thread creation), `memory.max` (the root shows the OOM killer's
  limit and measurement; a child's usage is its members' RSS, checked periodically by the OOM
  killer and immediately when a lower limit is written; `memory.oom.group` widens the kill),
  `cgroup.freeze` (threads stop before returning to guest code, waits return and restart after
  thaw; `cgroup.events` reports frozen once every task has stopped — a vfork parent waiting for
  its child never does), `cgroup.kill` (SIGKILL to every live process in the subtree),
  `cgroup.max.depth`/`max.descendants`, the device programs. Everything else (`memory.high`,
  `cpu.max`, `cpuset.cpus`, …) is format-checked and stored. `pids.current` counts live threads
  plus one per zombie group.
- `/proc/<pid>/cgroup` is `0::<path>` relative to the reader's cgroup-namespace root;
  `/proc/cgroups` reports hierarchy 0 for the v1 controller names.

### bpf(2) and device programs

- Commands: program load, attach, detach, query, get-fd-by-id, info by fd (no maps or links).
  When `unprivileged_bpf_disabled` is nonzero (default 2) every command needs `CAP_BPF` or
  `CAP_SYS_ADMIN`; loading a device program always needs one of them and also `CAP_NET_ADMIN` or
  `CAP_SYS_ADMIN` (6.1's `is_net_admin_prog_type`, `EPERM`); query needs `CAP_NET_ADMIN`, get-fd-by-id
  `CAP_SYS_ADMIN`; attach and detach check nothing on the target cgroup fd (any cgroup directory
  fd, `O_PATH` included), and detaching a non-multi program needs no program fd.
- Only `BPF_PROG_TYPE_CGROUP_DEVICE`. The verifier allows forward jumps only and requires every
  instruction to be reachable; no maps, bpf-to-bpf calls or atomics; the stack holds scalars;
  the context is read-only (12 bytes, aligned); pointer arithmetic is limited to constant
  offsets; R0 must be 0 or 1 at exit; seven helpers. The interpreter treats any out-of-range access
  as returning 0 (deny).
- Programs live while an fd or a cgroup holds them; attach adds to the cgroup's list. At device
  open (except rdev 0) and mknod (after `CAP_MKNOD`, whiteouts exempt), every program on the path
  from the caller's cgroup to the root runs; any denial is `EPERM`.

## Dependencies (inbound / outbound)

### Inbound

- `kernel/vfs` (walker and magic links, pseudo open/read/write/lseek/stat/getdents, chmod/chown/
  utimensat on devpts nodes, mkdir/rmdir in cgroup2, mount options for devpts and cgroup2, device
  checks); `kernel/process` (clone/clone3 cgroup placement and pids charge, unshare/setns, the
  freezer stop in the signal path); the OOM killer (`memory_limits`, `measure_memory`,
  `oom_group`, events); `syscall/poll.rs` (epoll refusal); the machine (`Offline` before boot);
  `kernel/signal` (`recorded_value` for `debug.exception-trace`, `mark_deleted` for the file
  named in unhandled-exception reports).

### Outbound

- Kernel state: thread groups and tasks, credentials (`permission`, `may_ptrace`, `capable`,
  `uid_out`/`gid_out`), pid namespaces (`vnr`, `find`), nsproxy, `mm` (sizes, residency, argument
  ranges, pagemap, forced writes), `signal`, `tty` (`Tty` owner/mode/times, `new_pty`), `netdev`/
  `netconf`/`route`/`net`/`unix`/`traffic`, `ipc` tunables, `mqueue` limits, `seccomp`, `klog`,
  `loadavg`, the VFS (`mounts_from`, `reach`, `path_of`, devpts options, the loop devices'
  `Loops`, `proc_locks` and `fd_locks`), the OOM killer (`proc_score`).
- Host: `appine_hal` (`host_meminfo`, `host_vmstat`, `host_swaps`, `host_cpus`, `cpu_sysfs`,
  `clock_gettime`, `fill_random`, `thread_ctxt_switches`, routes, `tx_queue_len`); the engine
  (`guest_hwcap*`, `guest_midr`).

## Invariants and gotchas

- Content always reflects the kernel at generation time; nothing is cached across opens, and a
  regeneration uses the opener's credentials but the current reader's namespaces.
- `/proc/sys` lookups use the reader's namespaces; `/proc/<pid>/net` uses the target's.
- Synthetic nodes have no store inode, so store-only operations (chown/chmod outside devpts,
  xattrs, links) fail on them.
- `dev_ino` (epoll fdinfo) uses the hashed inode and the boot instance's device, which can disagree
  with stat for pid nodes, later procfs instances and nsfs files.

## Confirmed bugs / technical debt

- **Per-thread fd views are wrong:** `task/<tid>/fd` and `fdinfo` show the group leader's table even
  when the thread has its own (clone without `CLONE_FILES`, `unshare(CLONE_FILES)`).
- **No change notifications:** synthetic files cannot be added to epoll (`EPERM`) and always poll
  ready, so mount table changes (`mountinfo`'s `POLLPRI`) and `cgroup.events` changes are never
  signalled; libmount monitors, systemd and containerd's OOM watching rely on them.
- **`frozen 1` never appears while a vfork parent waits:** the waiting parent never counts as frozen,
  so runtimes that wait for `cgroup.events` to report frozen (runc pause) can wait until thaw.
- **Regeneration uses the current reader's namespaces:** a file reread from offset 0 is generated in
  the reading task's namespaces, while Linux binds a network or IPC sysctl to the namespace it was
  looked up in, so a descriptor of `/proc/sys/net/*` or an IPC limit passed to a process in another
  namespace shows that namespace's value.
- **Fidelity gaps:** modes and owners differ (`environ` 0444, `fd/` and `ns/` 0555, `comm`
  read-only, `fdinfo/<n>` and `ns/*` owned by root, devpts root 0555, a master's fstat fixed);
  `/sys/class/net` follows the reader rather than the mounter's network namespace; procfs mount
  options are ignored; chown delegation of cgroups is impossible; no threaded cgroups; many
  standard files are absent; `kernel/pid_max` writes are ignored; `status` has no
  `Cpus_allowed` mask line; cgroup directories are always 0755, ignoring the mkdir mode, so a
  private 0700 cgroup is searchable by everyone.
- **No `/sys/dev/block` or block `uevent`s:** tools that go from a device number to sysfs
  (`tst_find_backing_dev`, so LTP ioctl_loop05; udev-style lookups) find nothing, and the root
  filesystem has no block device behind it at all.
- **Socket owners in `/proc/net/*` can differ from Linux:** the uid column is the fsuid of the
  first process (lowest tid) holding the socket in its fd table, where Linux shows the creator's
  (`sock_i_uid`); sockets passed on, or whose creator closed its copy, show another uid, and
  sockets only in `SCM_RIGHTS` messages show 0.
- **`/proc/zoneinfo`, `/proc/pressure/*` and `/proc/kallsyms` are absent:** low-memory daemons
  (systemd-oomd and oomd need PSI's `/proc/pressure/memory` and its poll triggers), zone readers
  and symbol lookups (perf, bpftrace) find nothing.
- **`stat` address fields ignore the ptrace check:** `/proc/<pid>/stat` reports the code, stack,
  data and brk addresses as 0 for everyone and the argument and environment addresses to everyone,
  where Linux 6.1 shows real values only to readers passing the ptrace-read check (others see 1/1
  for code and 0 elsewhere), so debuggers and profilers reading the layout get zeros and any user
  learns other processes' argument and environment addresses.
- **A reaped process's held directory behaves like a live or missing one:** through a kept
  `/proc/<pid>` fd, lookups give `ENOENT` (Linux `ESRCH`), listing still succeeds (Linux
  `ENOENT`) and the first read of an open file returns the content from open (Linux `ESRCH`); as
  the fd is resolved by number, it shows the new process once the pid is reused, defeating monitors
  that hold `/proc/<pid>` fds to avoid pid-reuse races.

## Open questions

- Should procfs honour `hidepid`/`subset` (containers sometimes mount with `hidepid=2`)?
- Should `meminfo`/`sysinfo` be clamped to cgroup `memory.max` (LXCFS style), or is following
  docker deliberate?
- Should `/sys/class/net` follow the mounter's network namespace, as Linux tags sysfs mounts?
- Is the single-app trust model meant to excuse the missing `fd/` access check, or should procfs
  follow Linux's ptrace gating fully?

## To verify

- Linux 6.1's permission rules for cgroup device program attach/detach (believed to need no
  capability since CAP_BPF; Appine checks nothing on the target cgroup) and for SysV IPC/mqueue
  sysctls in child user namespaces (believed to be refused until about 6.8).
- Whether reopening a deleted file through `/proc/<pid>/fd/N` (the open-file node) checks
  permission (pipes do not).
- Field-by-field fdinfo layouts against Linux (eventfd ids, timerfd formatting).
- The Linux 6.1 behaviour behind the items found by Starnix's syscall tests (a zombie leader's
  files, `proc_sys_permission`, write opens of read-only proc files, `ENODEV` from removed cgroups)
  was observed on a native 6.8 kernel and attributed to 6.1 from knowledge of the kernel; so was
  which write each kind of read-only file fails with.
