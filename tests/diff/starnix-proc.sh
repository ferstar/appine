# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's syscall tests on procfs, sysfs, sysctls and cgroups: per-process and system
# files, their permissions, /proc/sys, /sys/kernel, cpuinfo, pressure files, the cgroup v2 tree and
# its freezer. Run by tests/diff/gtest/run.sh, one process per test. Tests that differ for a
# reason recorded in a map, or that cannot be compared, are skipped, with the reason above them.
# kernel/procfs: no change notifications (polling cgroup.events never wakes, so the test hangs);
# writes through a removed cgroup's open files give the wrong result; fidelity gaps: chown
# delegation of cgroups is impossible and cgroup directories are always 0755
SKIP="CgroupTest.WriteToInterfaceFileAfterCgroupIsDeleted"
SKIP="$SKIP:CgroupTest.ChownDirectoryDoesNotPropagate:CgroupTest.NewDirectoryOwnedByCreator"
# kernel/procfs: /proc/zoneinfo, /proc/pressure/* and /proc/kallsyms are absent; per-process
# timerslack_ns is absent; Yama is absent; no tracefs
SKIP="$SKIP:ProcfsTest.ZoneInfo:PressureTest/*:ProcKallsymsTest.*:ProcTaskDirTest.TimerslackNs*"
SKIP="$SKIP:SysctlTest/SysctlNodeTest.*/kernel_yama_ptrace_scope:TracefsTest.BufferSizeKbReadWrite"
# kernel/procfs: /proc/<pid>/fd and fdinfo are open to everyone
SKIP="$SKIP:ProcfsAccessTest/ProcfsAccessTest.AccessDeniedToNonOwner/fd"
SKIP="$SKIP:ProcfsAccessTest/ProcfsAccessTest.AccessDeniedToNonOwner/fdinfo"
SKIP="$SKIP:ProcfsAccessTest/ProcfsAccessTest.AccessDeniedToNonOwnerWithoutCapPtrace/fdinfo"
# kernel/procfs fidelity gaps: st_blksize is 4096; procfs mount options are ignored; fd links are
# 0777 whatever the open mode
SKIP="$SKIP:ProcfsBlksizeTest.*:ProcHidePidTest.HidePidAndGidMountOptions:ProcTaskDirTest.FdOpathMode"
# kernel/procfs: a reaped process's held directory behaves like a live or missing one
SKIP="$SKIP:ProcTaskDirTest.PidDirFdListingAndReadsFailAfterExit"
SKIP="$SKIP:ProcTaskDirTest.PidDirFdLookupsFailAfterExit"
# kernel/procfs: /proc/sys permissions follow ordinary DAC, and CAP_NET_ADMIN does not give root's
# access to net.*
SKIP="$SKIP:SysctlTest/SysctlNodeTest.NonRootWithCapDacOverrideCannotWrite/*"
SKIP="$SKIP:SysctlTest/SysctlNodeTest.NonRootWithCapNetAdminCanWriteProcSysNet/net_*"
# No perf_event_open (ENOSYS, kernel/signal D10) and no tun device (kernel/net D1)
SKIP="$SKIP:ProcSelfFdTest.PerfEventFdName:SysctlTest.DisableIpv6Default"
# The io_uring sysctls are 6.6's, and the contract is 6.1
SKIP="$SKIP:SysctlTest/SysctlNodeTest.*/kernel_io_uring_*"
# Cannot be compared: the native side runs in docker's network namespace, which has no
# net.core.rmem_max and wmem_max (Linux registers them for the initial namespace only)
SKIP="$SKIP:SysctlTest/SysctlNodeTest.*/net_core_rmem_max:SysctlTest/SysctlNodeTest.*/net_core_wmem_max"
SKIP="$SKIP:SysctlTest/SysctlTestReadBack.ReadBack/core_rmem_max_*"
SKIP="$SKIP:SysctlTest/SysctlTestReadBack.ReadBack/core_wmem_max_*"
# Cannot be compared: pid 2 is kthreadd only on a whole system; docker's pid namespace has no pid 2
# and Appine gives it to an ordinary process
SKIP="$SKIP:ProcTaskDirTest.Kthread*"
sh /tmp/difftest-files/gtest/run.sh starnix "$SKIP" procfs_test sysctl_test sysfs_kernel_test \
    cpuinfo_test pressure_test cgroup_test cgroup_freezer_test
