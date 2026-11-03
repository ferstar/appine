# Synthetic filesystems (kernel/procfs) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/procfs"`
> Last distilled: 2026-11-03 (HEAD = 9e5be2b) — seeded by hand from the retired design doc §8.2 and
> the round records in `docs/rounds/`; no `MODULE: kernel/procfs` Decision blocks exist yet, so
> sources cite rounds instead of commits.

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
- **Why**: These are what `docker run --pids-limit`/`--memory`, podman's equivalents, `docker pause` and systemd's `TasksMax=`/`MemoryMax=` write. Host threads' scheduling is not under appine's control, so `cpu.max`, `cpu.weight` and `cpuset.cpus` have nothing to act on.
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
