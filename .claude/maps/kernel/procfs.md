# kernel/procfs Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/procfs.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 5 research concerns; 21 claims checked by 3 independent verifiers — 14 confirmed and 7 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-07 (targeted verification only since the full mapping: the socket owner column of `/proc/net/*`; `/proc/<tid>` resolves any thread; module references; `/sys/block` for loop devices; `vm.mmap_min_addr`, VmStk, newlines in maps; bugs from Starnix's syscall tests; recorded sysctls; the exited main thread; `/proc/vmstat`)

## Responsibilities

`kernel/procfs` provides the synthetic filesystems, generated live from kernel state with nothing
stored:

- procfs: the per-process and per-thread trees, system-wide files, `/proc/sys` (sysctls, many
  scoped by namespace) and `/proc/<pid>/net`;
- sysfs: CPU topology (partly copied from the host), `/sys/class/net`, `/sys/block` (loop
  devices), the cgroup mount point;
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
- `Global`/`GLOBALS` (13 top-level files), `Entry`/`ENTRIES` (32 per-process entries),
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
  readers not visible in the instance. Numeric names (pids, fds) are parsed as signed 64-bit
  decimals of at most 10 characters without a leading zero, so a sign is accepted, and fd names
  are then truncated to 32 bits.
- Inode numbers hash the node (low bit set); roots are 1; nodes under a pid mix in the thread
  group's serial, so a reused pid gets new inodes; nsfs files use the namespace id. Inodes are
  stable within one run, not across runs.
- Modes: directories 0555 (0755 on sysfs and cgroup), symlinks 0777, `mem` 0600, recorded
  sysctls Linux's (`vm.drop_caches` 0200, `vm.mmap_rnd_bits` 0600), other writable files 0644, `pagemap`, `auxv` and kmsg 0400, everything else 0444 (so `fd/`, `fdinfo/`, `ns/` are 0555,
  `environ` 0444, `comm` and `projid_map` read-only); cgroup files use their own table modes,
  `NetSysctl` files theirs, devpts nodes the terminal's mode and `ptmxmode`. Owners: per-process
  entries (including the `fd/`, `fdinfo/`, `ns/`, `net/` directories) belong to the process's
  euid/egid, or the root of its credentials' user namespace when not dumpable; the entries inside
  `fdinfo/`, `ns/` and `net/`, reopened open files and everything else belong to root.
  Times are "now" except devpts terminals; directories have nlink 2 (`task/` 2 plus threads);
  sizes are 0 except sysfs and cgroup files (4096).

### Opening, reading and writing

- Open (in `kernel/vfs`'s pseudo open): DAC against the synthetic stat; for `mem`, `auxv`,
  `environ`, `maps`, `smaps`, `smaps_rollup` and `pagemap` one ptrace check with real credentials
  and no read/attach distinction (`EACCES`), while the magic-link check (`may_read`) uses
  filesystem credentials; `fd/` and `fdinfo/` have no ptrace check at all; devpts nodes on nodev
  mounts `EACCES`, then the cgroup device filter; write modes on ro mounts `EROFS`, on
  non-writable nodes `EACCES`; creating names `EACCES`.
- The content is generated once at open (`fresh`). The first read at offset 0 uses it; later reads
  or preads at offset 0 regenerate it with the current reader's view (credentials and
  namespaces); reads elsewhere use the current snapshot; lseek (`SEEK_SET`/`CUR`/`END` against the
  snapshot length) never regenerates. `pagemap`, `mem` and `/proc/kmsg` are computed at read time;
  nsfs files cannot be read (`EINVAL`) and, like write-only cgroup files, never regenerate; a
  `NetConf` file whose generation fails at open reports the error on the first read.
- Writes pass the whole buffer of each call to `pseudo::write` with the writer's credentials and
  namespaces, apply immediately, do not advance the position (except `mem`), and mark the snapshot
  stale.
  Per-process numbers use `proc_number`/`kstrto` (first 12 bytes, base 0, `EINVAL`/`ERANGE`);
  most sysctls parse decimal after trimming.
- Synthetic files (other than kmsg) cannot be added to epoll (`EPERM`) and poll as always ready, so
  mount table changes are never signalled through mountinfo.

### Per-process files

- Entries: `auxv`, `stat`, `status`, `statm`, `cmdline`, `comm`, `environ`, `limits`, `maps`,
  `mem`, `smaps`, `smaps_rollup`, `pagemap`, `mounts`, `mountinfo`, `cwd`, `exe`, `root`, `fd/`,
  `fdinfo/`, `task/`, `net/`, `oom_score_adj`, `coredump_filter`, `cgroup`, `ns/`, `uid_map`,
  `gid_map`, `projid_map`, `setgroups`, `timens_offsets`; `task/<tid>` has the same minus `task`,
  `pagemap`, `oom_score_adj`, `coredump_filter`, `timens_offsets`, plus `children`. Absent:
  `oom_score`, `oom_adj`, `io`, `wchan`, `sched*`, `personality`, `loginuid`/`sessionid`,
  `attr/`, `stack`, `syscall`, `cpuset`, `mountstats`, `timerslack_ns`.
- `stat`: all 52 fields; fault counters, code/stack addresses, `wchan`, `processor`, priority and
  policy are 0; start time in clock ticks plus the reader's time-namespace boottime offset; the
  thread's or the group's CPU times depending on the view; `tty_nr` from the controlling terminal.
- `status`: ids translated into the reader's user namespace, `NStgid`/`NSpid`/`NSpgid`/`NSsid`
  from the instance's namespace down, capabilities, `NoNewPrivs`, `Seccomp`, signal masks; memory
  lines approximate (`VmPeak` = `VmSize` = `VmData`, `VmHWM` from max RSS, `VmStk` the grows-down
  mappings, exe/lib/PTE/swap 0; omitted with no live task); `FDSize` rounded to 64;
  `Cpus_allowed_list` lists every guest CPU. `maps` and `smaps` escape a newline in a mapped
  file's path as `\012` (Linux's `seq_file_path`).
  `statm` is `size rss 0 0 0 rss 0`.
- `cmdline`/`environ` copy the argument and environment ranges of the current mm (empty over
  1 MiB or when unreadable); `comm` is read-only; `limits` lists the rlimits.
- `fd/` lists the fds of the group leader's table (also under `task/<tid>`, even for a thread with
  its own table); `fd/N` links show the
  path stored at open with ` (deleted)` appended when nlink is 0; `fdinfo/N` has `pos`, `flags`
  (octal, with `O_CLOEXEC`), `mnt_id`, `ino` and per-kind lines (eventfd count, inotify watches,
  epoll `tfd:` lines, timerfd clock and times, signalfd mask, pidfd pids).
- `mounts`/`mountinfo` come from the VFS mount table for the process's namespace and root (a
  zombie falls back to the initial namespace); `cgroup` is the path relative to the reader's
  cgroup-namespace root; `net/` uses the target's network namespace.
- Writable: `oom_score_adj` (-1000..1000; going below the minimum needs `CAP_SYS_RESOURCE`, and a
  privileged write sets the new minimum), `coredump_filter` (masked to 0x1ff), `mem`, the id maps,
  `setgroups`, `timens_offsets`.
- Zombies keep their group without tasks: stat/status work (state Z), cmdline/environ/auxv are
  empty, `fd/` is empty, `cwd`/`root`/`exe`/`ns` are `ENOENT`. A main thread that has exited while
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
  `softirq`, `procs_blocked` are 0; `processes` counts new processes (not threads). `uptime`
  includes the reader's
  boottime offset. `loadavg` comes from the guest's own load average. `filesystems` is a fixed
  list; `cmdline` is `appine`; `cgroups` from the cgroup tree. `mounts` and `net` link into
  `self`.
- Absent: `swaps`, `partitions`, `devices`, `locks`, `sys/fs/file-nr`,
  `protected_regular`/`protected_fifos`, Yama.
- Sysctl scoping: SysV IPC limits and `fs/mqueue/*` per the writer's IPC namespace; `hostname`/
  `domainname` per UTS namespace (one trailing newline stripped, over 64 bytes `EINVAL`);
  `ns_last_pid` per pid namespace; `net.*` per network namespace (`ip_forward` aliases
  `conf/all/forwarding`; `NetConf` directories per interface); everything else kernel-wide
  (`max_map_count`, `mmap_min_addr`, inotify limits, `aio-max-nr`, `lease-break-time`, `nr_open`,
  `pipe-max-size`, `core_pattern`, `core_uses_pid`, `protected_symlinks`/`hardlinks`, `printk*`,
  `panic`, `user/max_*_namespaces`; `dmesg_restrict`, `unprivileged_bpf_disabled` and seccomp
  `actions_logged` need `CAP_SYS_ADMIN` in the initial namespace, `mmap_min_addr`
  `CAP_SYS_RAWIO`).
- Several writable sysctls are recorded without effect (`kernel.panic`, `user.max_*`, every
  `NetSysctl` entry, `ping_group_range`, `somaxconn`, `ip_local_port_range`; of the port sysctls
  only `ip_unprivileged_port_start` is used, by bind); `kernel/pid_max` accepts writes and ignores
  them. The `RECORDED` table adds 40 more with Linux 6.1's modes, defaults and accepted ranges
  (`overflowuid`/`gid`, `vm.dirty_*`, `drop_caches`, `swappiness`, `overcommit_ratio`,
  `page-cluster`, `watermark_scale_factor`, `mmap_rnd_*bits`, `kernel.sysrq`,
  `randomize_va_space` (default 0: nothing is randomized), `kptr_restrict` (writes need
  `CAP_SYS_ADMIN`), `perf_*`, `hung_task_*`, `sched_rt_*`, `sched_schedstats`, `panic_on_oops`,
  `core_pipe_limit`, `modules_disabled` (only 1 can be written), `fs.suid_dumpable`,
  `debug.exception-trace`), kept kernel-wide in `Kernel::sysctls`; `drop_caches` is write-only
  (reads `EINVAL`). `kernel.modprobe` keeps a string, `kernel.tainted` only gains flags (of the 19
  defined; writes need `CAP_SYS_ADMIN`), and `vm.overcommit_memory` (0 to 2) is used: 1 turns off
  fork's commit check (`kernel/mm`). Constants: `file-max`, `cap_last_cap` 40, `ngroups_max`,
  `pid_max` 4194304; `random/uuid` is fresh per read, `boot_id` fixed per boot, the entropy values
  256.
- Write permission is plain DAC against root-owned files (0644 when writable) with no
  per-namespace grants, so `CAP_DAC_OVERRIDE` counts only where root is mapped in the writer's
  user namespace, and `user/max_*` is one kernel-wide table (Linux 6.1 keeps it per user
  namespace).
- `/proc/<pid>/net/*` (`arp`, `dev`, `icmp`, `icmp6`, `if_inet6`, `ipv6_route`, `netstat`, `raw`,
  `raw6`, `route`, `snmp`, `sockstat`, `sockstat6`, `tcp`, `tcp6`, `udp`, `udp6`, `unix`) uses the
  target task's network namespace: sockets from every fd table plus in-flight AF_UNIX sockets,
  filtered by namespace, uids mapped for the reader; host interface counters and routes only in
  the initial namespace (new namespaces show zeros); `arp` is a header line.

### sysfs

- Tree: `block/`, `class/net/`, `devices/system/cpu/` (`online`, `possible`, `present`, `offline`, one
  `cpu<n>/` per guest CPU with synthesized `topology/` — each CPU its own core, one package — and
  `regs/identification/`), `cache/` and `cpufreq/` files copied from the host when the host lets
  the app read them (CPU numbers translated, missing files absent), and the empty `fs/cgroup/`
  mount point. No `kernel/`, `module/`, `dev/`, `bus/`, `firmware/`, `power/`, NUMA nodes,
  other classes or `uevent` files.
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
  target is freezing; the target only has to exist (`ENODEV`), lie with the parent's cgroup inside
  the caller's cgroup-namespace root (`ENOENT`) and have no controllers enabled for children
  (`EBUSY`) — there is no permission check. Zombies stay until reaped; `CLONE_NEWCGROUP`/unshare
  roots the new namespace at the current cgroup. The BKL is held from the charge until the child
  is registered, so no fork escapes a concurrent `cgroup.kill`.
- `cgroup.procs` writes: pid 0 is the caller; pids resolve in the writer's pid namespace
  (`ESRCH`); kernel init `EINVAL`; source or destination outside the writer's cgroup-namespace
  root `ENOENT`; a non-root destination with controllers enabled for children `EBUSY`; then the
  group moves and freezing is re-evaluated. Only the destination file's open permission is
  checked (no common-ancestor check as in Linux's `cgroup_procs_write_permission`). Reads of
  `cgroup.procs`/`cgroup.threads` print global ids, untranslated. `cgroup.threads` only accepts
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
  `CAP_SYS_ADMIN`; loading a device program always needs one of them (Linux 6.1 also wants
  `CAP_NET_ADMIN` or `CAP_SYS_ADMIN` for this type); query needs `CAP_NET_ADMIN`, get-fd-by-id
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
  `oom_group`, events); `syscall/poll.rs` (epoll refusal); the machine (`Offline` before boot).

### Outbound

- Kernel state: thread groups and tasks, credentials (`permission`, `may_ptrace`, `capable`,
  `uid_out`/`gid_out`), pid namespaces (`vnr`, `find`), nsproxy, `mm` (sizes, residency, argument
  ranges, pagemap, forced writes), `signal`, `tty` (`Tty` owner/mode/times, `new_pty`), `netdev`/
  `netconf`/`route`/`net`/`unix`/`traffic`, `ipc` tunables, `mqueue` limits, `seccomp`, `klog`,
  `loadavg`, the VFS (`mounts_in`, `reach`, `path_of`, devpts options, the loop devices'
  `Loops`).
- Host: `appine_hal` (`host_meminfo`, `host_cpus`, `cpu_sysfs`, `clock_gettime`, `fill_random`,
  `thread_ctxt_switches`, routes, `tx_queue_len`); the engine (`guest_hwcap*`, `guest_midr`).

## Invariants and gotchas

- Content always reflects the kernel at generation time; nothing is cached across opens, and a
  regeneration uses the current reader's credentials and namespaces (Linux keeps the opener's
  `f_cred`).
- `/proc/sys` lookups use the reader's namespaces; `/proc/<pid>/net` uses the target's.
- Synthetic nodes have no store inode, so store-only operations (chown/chmod outside devpts,
  xattrs, links) fail on them.
- `dev_ino` (epoll fdinfo) uses the hashed inode and the boot instance's device, which can disagree
  with stat for pid nodes, later procfs instances and nsfs files.

## Confirmed bugs / technical debt

- **`CLONE_INTO_CGROUP` has no permission check:** any process can `clone3` a child into any cgroup
  inside its cgroup namespace through an `O_PATH` directory fd, escaping its own cgroup's
  `pids.max`, `memory.max`, freezer and device programs (Linux 6.1 requires write permission on
  the target's `cgroup.procs` and the migration permission rules).
- **`/proc/<pid>/fd` and `fdinfo` are open to everyone:** listing, readlink, following and fdinfo
  skip the ptrace-read check (the directories are 0555, Linux 0500 plus the check), so any user
  sees and follows other users' open files and positions; reopening a pipe through `fd/N` checks no
  permission at all. See the matching `kernel/vfs` item.
- **Delegated cgroups can capture other users' processes:** writing `cgroup.procs` checks only the
  destination file, so the owner of a cgroup (one created for them by a privileged process) can
  move any process, root's included, into it and then freeze or kill it through `cgroup.freeze` /
  `cgroup.kill` (Linux requires write access to the common ancestor's `cgroup.procs`).
- **Numeric names alias:** fd names are truncated to 32 bits (`/proc/self/fd/4294967296` is fd 0)
  and signs are accepted (`/proc/self/fd/+1`, `/proc/+1`), where Linux returns `ENOENT`.
- **Per-thread fd views are wrong:** `task/<tid>/fd` and `fdinfo` show the group leader's table even
  when the thread has its own (clone without `CLONE_FILES`, `unshare(CLONE_FILES)`).
- **`cgroup.procs`/`cgroup.threads` show global ids:** readers in a child pid namespace (a container
  managing its own cgroups) see ids that do not exist for them; writes translate correctly.
- **No change notifications:** synthetic files cannot be added to epoll (`EPERM`) and always poll
  ready, so mount table changes (`mountinfo`'s `POLLPRI`) and `cgroup.events` changes are never
  signalled; libmount monitors, systemd and containerd's OOM watching rely on them.
- **`frozen 1` never appears while a vfork parent waits:** the waiting parent never counts as frozen,
  so runtimes that wait for `cgroup.events` to report frozen (runc pause) can wait until thaw.
- **Namespace root cannot write its sysctls:** root of a child user namespace that owns a network
  namespace cannot write that namespace's `net.*` sysctls, and `user/max_*` is global (Linux 6.1
  grants both per namespace); rootless container runtimes setting sysctls fail with `EACCES`.
- **Open-time ptrace check uses real credentials:** `mem`, `environ`, `maps` and the others are
  checked with real ids where Linux uses filesystem ids (and attach mode for `mem`), so set-id
  helpers behave differently (also noted in `kernel/signal`).
- **Regeneration uses the current reader:** a file reread from offset 0 is generated with the
  reading task's credentials and namespaces, not the opener's (Linux's `f_cred`), so a descriptor
  passed between processes changes content (id maps, `/proc/sys`).
- **bpf load capability:** loading a device program accepts `CAP_BPF` alone; Linux 6.1 also requires
  `CAP_NET_ADMIN` or `CAP_SYS_ADMIN` for this program type.
- **Fidelity gaps:** modes and owners differ (`environ` 0444, `fd/` and `ns/` 0555, `comm`
  read-only, `fdinfo/<n>` and `ns/*` owned by root, devpts root 0555, a master's fstat fixed);
  `/sys/class/net` follows the reader rather than the mounter's network namespace; procfs mount
  options are ignored; chown delegation of cgroups is impossible; no threaded cgroups; many
  standard files are absent; `kernel/pid_max` writes are ignored; `processes` in `/proc/stat`
  excludes threads; `Cpus_allowed_list` ignores task affinity; `fd/<n>` links are 0777 instead
  of encoding the open mode (0500, 0300, 0700, 0 for `O_PATH`); `st_blksize` is 4096 instead of
  1024; cgroup directories are always 0755, ignoring the mkdir mode, so a private 0700 cgroup is
  searchable by everyone.
- **No `/sys/dev/block` or block `uevent`s:** tools that go from a device number to sysfs
  (`tst_find_backing_dev`, so LTP ioctl_loop05; udev-style lookups) find nothing, and the root
  filesystem has no block device behind it at all.
- **Socket owners in `/proc/net/*` can differ from Linux:** the uid column is the fsuid of the
  first process (lowest tid) holding the socket in its fd table, where Linux shows the creator's
  (`sock_i_uid`); sockets passed on, or whose creator closed its copy, show another uid, and
  sockets only in `SCM_RIGHTS` messages show 0.
- **Read-only proc files cannot be opened for writing even with `CAP_DAC_OVERRIDE`:** every
  write-mode open of a node that is not writable is `EACCES` before capabilities are considered,
  where Linux 6.1 lets root open ordinary `/proc` files (not `/proc/sys` or sysfs) with `O_WRONLY`,
  `O_RDWR` or `O_TRUNC` and fails only the write (`EIO` for `/proc/kmsg`); C++ `std::fstream` with
  its default mode or `fopen(..., "r+")` on `/proc/cpuinfo` or `/proc/self/status` fails outright
  as root, and a root `/proc/kmsg` reader opening it read-write gets no descriptor.
- **`/proc/sys` permissions follow ordinary DAC:** a non-root writer with `CAP_DAC_OVERRIDE` can
  open every writable sysctl (`core_pattern`, `hostname`, `pid_max`, `net.*`), whereas Linux 6.1
  decides by euid 0 / egid 0 alone and ignores capabilities; and a non-root process with
  `CAP_NET_ADMIN` cannot write `net.*` sysctls (Linux gives it root's access; `user.*` likewise
  keys on `CAP_SYS_RESOURCE`), so network daemons that drop root but keep `CAP_NET_ADMIN` get
  `EACCES`.
- **`/proc/zoneinfo`, `/proc/pressure/*` and `/proc/kallsyms` are absent:** low-memory daemons
  (systemd-oomd and oomd need PSI's `/proc/pressure/memory` and its poll triggers), zone readers
  and symbol lookups (perf, bpftrace) find nothing.
- **`stat` address fields ignore the ptrace check:** `/proc/<pid>/stat` reports the code, stack,
  data and brk addresses as 0 for everyone and the argument and environment addresses to everyone,
  where Linux 6.1 shows real values only to readers passing the ptrace-read check (others see 1/1
  for code and 0 elsewhere), so debuggers and profilers reading the layout get zeros and any user
  learns other processes' argument and environment addresses.
- **Non-UTF-8 thread names are rewritten:** `comm`, `stat` and `status` replace invalid UTF-8 in the
  thread name with U+FFFD, so a name cut mid-character by the 15-byte limit, or any non-UTF-8
  `PR_SET_NAME`, reads back differently from what was set (Linux prints the raw bytes) and
  `pgrep`/`pidof` or self-checks fail to match.
- **`status` lacks `RssAnon`/`RssFile`/`RssShmem`:** memory monitors and runtimes that split
  resident memory by type from `/proc/<pid>/status` find no such lines.
- **A reaped process's held directory behaves like a live or missing one:** through a kept
  `/proc/<pid>` fd, lookups give `ENOENT` (Linux `ESRCH`), listing still succeeds (Linux
  `ENOENT`) and the first read of an open file returns the content from open (Linux `ESRCH`); as
  the fd is resolved by number, it shows the new process once the pid is reused, defeating monitors
  that hold `/proc/<pid>` fds to avoid pid-reuse races.
- **Writes through a removed cgroup's open files give the wrong result:** after rmdir,
  `cgroup.procs`, `cgroup.freeze` and value files fail with `ENOENT` and `cgroup.kill` succeeds
  silently, where Linux 6.1 fails every write with `ENODEV`, which container managers treat as
  "cgroup gone".

## Open questions

- Should procfs honour `hidepid`/`subset` (containers sometimes mount with `hidepid=2`)?
- Should `meminfo`/`sysinfo` be clamped to cgroup `memory.max` (LXCFS style), or is following
  docker deliberate?
- Should `/sys/class/net` follow the mounter's network namespace, as Linux tags sysfs mounts?
- Is the single-app trust model meant to excuse the missing `fd/` access check, or should procfs
  follow Linux's ptrace gating fully?

## To verify

- Linux 6.1's permission rules for cgroup device program attach/detach (believed to need no
  capability since CAP_BPF; appine checks nothing on the target cgroup) and for SysV IPC/mqueue
  sysctls in child user namespaces (believed to be refused until about 6.8).
- Whether reopening a deleted file through `/proc/<pid>/fd/N` (the open-file node) checks
  permission (pipes do not).
- Field-by-field fdinfo layouts against Linux (eventfd ids, timerfd formatting).
- The Linux 6.1 behaviour behind the items found by Starnix's syscall tests (a zombie leader's
  files, `proc_sys_permission`, write opens of read-only proc files, `ENODEV` from removed cgroups)
  was observed on a native 6.8 kernel and attributed to 6.1 from knowledge of the kernel.
