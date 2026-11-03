# appine design

- **Status**: the design decisions were confirmed one by one in the design review of 2026-10-24. M0–M8 are done; M9 (release) is in progress, with real-device validation, the license obligations and the Play submission still to go (§14.1).
- **Scope**: v1. Sections 1–13 describe how things work now and are updated with each round's changes; how a change came about, its measurements and its sources are recorded in the rounds (written in Chinese, indexed in [milestones.md](../milestones.md)). Subsystems are moving section by section to their module's map under `.claude/maps/` (modules are listed in [MODULES.md](../../.claude/MODULES.md)); a migrated section is removed from here and the section numbers stay.
- **In one sentence**: a **single-process** Android app that can ship on Google Play and runs unmodified Alpine Linux aarch64 user-space programs in its own process, with an interpreter and a same-ISA binary translator. Same idea as iSH; its Android counterpart.

## Contents

1. [Goals and non-goals](#1-goals-and-non-goals)
2. [Hard constraints](#2-hard-constraints)
3. [Overall architecture](#3-overall-architecture)
4. CPU engine, moved to the [engine map](../../.claude/maps/engine.md)
5. Memory: a lightweight user-space soft-MMU, moved to the [kernel/mm map](../../.claude/maps/kernel/mm.md)
6. Processes, threads and scheduling, moved to the [kernel/process map](../../.claude/maps/kernel/process.md)
7. Signals and ABI, moved to the [kernel/signal map](../../.claude/maps/kernel/signal.md)
8. File system, moved to the [kernel/store](../../.claude/maps/kernel/store.md), [kernel/vfs](../../.claude/maps/kernel/vfs.md) and [kernel/procfs](../../.claude/maps/kernel/procfs.md) maps
9. TTY and terminal, moved to the [kernel/tty map](../../.claude/maps/kernel/tty.md); the terminal UI is in the [android/terminal map](../../.claude/maps/android/terminal.md)
10. Networking, moved to the [kernel/net](../../.claude/maps/kernel/net.md), [kernel/netns](../../.claude/maps/kernel/netns.md), [kernel/unix](../../.claude/maps/kernel/unix.md) and [kernel/poll](../../.claude/maps/kernel/poll.md) maps
11. Users, permissions and Android integration, moved to the [kernel/cred](../../.claude/maps/kernel/cred.md), [android/service](../../.claude/maps/android/service.md), [android/setup](../../.claude/maps/android/setup.md), [android/documents](../../.claude/maps/android/documents.md), [android/terminal](../../.claude/maps/android/terminal.md) and [android/native](../../.claude/maps/android/native.md) maps
12. [Threat model](12-threat-model.md)
13. [Engineering](13-engineering.md)
14. [Milestones](14-milestones.md)
15. [Rejected approaches](15-rejected.md)
16. [Risks and assumptions to validate](16-risks.md)

---

## 1. Goals and non-goals

### 1.1 Goals

- A user installs the app from Play, opens it and gets an Alpine shell: no root, no Termux, no extra steps.
- The whole Linux environment runs inside the app's own process, so Android 12+'s phantom process killer does not apply to it.
- **Unmodified** binaries from Alpine's official aarch64 repositories run directly.

### 1.2 v1 workload classes

| Class | Workloads | v1 requirement |
|---|---|---|
| A everyday CLI | busybox ash, `apk`, coreutils, vim, git, curl, openssh client | **all usable** |
| B compute-heavy | python3, gcc/make building medium-sized C projects, `./configure` | **runs to completion** |
| C difficult runtimes | static Go binaries, node/V8, JVM | **correct behavior, speed not required** |
| D not promised | containers, init systems, kernel modules, X11/GUI | — |

Classes A–C are all met. In class D, podman and docker (namespaces in the [kernel/process map](../../.claude/maps/kernel/process.md), the [kernel/netns map](../../.claude/maps/kernel/netns.md)) and OpenRC (PID 1 in the [kernel/process map](../../.claude/maps/kernel/process.md)) actually work; systemd, kernel modules and GUI are out of scope. strace and gdb work ([kernel/signal map](../../.claude/maps/kernel/signal.md)).

The default distribution is Alpine; settings can switch it to Debian 13 ([android/setup map](../../.claude/maps/android/setup.md)). glibc programs (manylinux wheels, Node.js prebuilt modules, rustup, OpenJDK) run without changes to the kernel layer.

### 1.3 Performance goals

- CPU-bound tasks within **3–5×** of native (JIT class). Current status: R1 in §16.
- Interactive latency comes first: `ls` and tab completion must not feel sluggish.

## 2. Hard constraints

| Constraint | Source | Consequence |
|---|---|---|
| **Google Play compliance** | Device and Network Abuse policy: an app may not download executable code (dex/JAR/.so), except "code that runs in a virtual machine or an interpreter" | Alpine binaries downloaded from the network **cannot execute directly on the host CPU**; the CPU back end can only be an interpreter or a translator |
| Single process | Design goal + phantom process killer | All guest processes share one host address space; isolation between guest processes and fork must be implemented in software |
| W^X execution restriction | With targetSdk ≥ 29, files in the app's data directory cannot be `execve`d | No execve, and no reliance on the host loader |
| Storage restrictions | SELinux: the app cannot create hard links, `chown` or `mknod`; shared storage is case-insensitive, has no permission bits and cannot hold symlinks | File system metadata and the namespace must be implemented by appine itself |
| Network restrictions | With targetSdk ≥ 30 the app cannot bind `NETLINK_ROUTE`, and `RTM_GETLINK` is blocked too; no `CAP_NET_RAW` | netlink is emulated in-process; ICMP goes through ping sockets |
| Page size | From Android 15, apps must be compatible with 16 KB page kernels | The page size presented to the guest must be decoupled from the host page size |
| Host address space | The common GKI configuration is 4K pages + 39-bit VA, giving user space only about 512 GB | A large contiguous address range cannot be reserved for each guest process |

The minimum system version is **minSdk 29 (Android 10)**: `android_res_nsend`, which the DNS stub relies on, is available from API 29.

## 3. Overall architecture

```mermaid
flowchart TB
    subgraph APP["Android app process (single process)"]
        UI["Kotlin shell<br/>Activity · foreground service · DocumentsProvider<br/>terminal UI (vendored Termux terminal-view)"]
        subgraph KERNEL["Kernel layer (Rust)"]
            SYS["syscall dispatch · processes/threads · signals · credentials · IPC"]
            MM["mm: VMA table + software page tables"]
            VFS["VFS: inode store · devpts · procfs"]
            TTY["tty/pty + line discipline"]
            NET["sockets: host passthrough · AF_UNIX · netlink · DNS stub"]
        end
        subgraph ENGINE["Engine (C + aarch64 assembly)"]
            T0["Tier 0 interpreter"]
            T1["Tier 1 same-ISA DBT"]
            SMMU["soft-MMU fast path (software TLB)"]
        end
        HAL["Host HAL: Android (bionic) / Linux (glibc, for development)"]
    end
    UI -- JNI --> KERNEL
    ENGINE -- "exit reasons: svc / fault / undef / brk / kick / code / host_fault" --> KERNEL
    KERNEL -- "cpu_run / dbt_run / mmu_map / tlb_flush / dbt_invalidate / kick" --> ENGINE
    KERNEL --> HAL
```

Layering principles:

- The **engine** only cares about executing guest instructions. On `svc`, a memory-access fault, an undefined instruction, `brk` or `ic ivau`, it hands control to the kernel layer.
- The **kernel layer** implements the Linux user-space ABI and holds all guest state: processes, address spaces, fds, file systems.
- The **host HAL** isolates host differences. The development host is Linux arm64 and the production host is Android; there is no macOS HAL.
- The core libraries (engine + kernel layer) **depend on no Android component**. Android-specific code appears only in the HAL and the Kotlin shell.

---

## References

- [Google Play: Device and Network Abuse policy](https://support.google.com/googleplay/android-developer/answer/9888379)
- [arm64: entry: Convert to Generic Entry v11 (2026-01, includes SUD)](https://lkml.iu.edu/2601.3/04930.html)
- [arm64 memory layout: 4KB pages + VA39 (LKML)](https://lore.kernel.org/lkml/YV3ZfmSHHKSWfDQu@arm.com/t/)
- [AOSP sepolicy: support hard linking for split APKs (removes link from create_file_perms)](https://android.googlesource.com/platform/external/sepolicy/+/9bef250)
- [termux-app LICENSE.md](https://github.com/termux/termux-app/blob/master/LICENSE.md)
- [azahar-emu/dynarmic (0BSD)](https://github.com/azahar-emu/dynarmic)
- iSH: <https://github.com/ish-app/ish> (GPLv3; only the ideas are borrowed, no code is copied)
