# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's syscall tests on processes and threads: execve and the ELF loader, exit, abort
# and zombies, thread-group lifecycle, waitpid flags and exit signals, pidfd, kcmp, futexes,
# scheduling, rlimits, prctl, sysinfo, POSIX timers, restarted reads, breakpoints, and the tests'
# own fork helpers. Run by tests/diff/gtest/run.sh, one process per test. Tests that differ for a
# reason recorded in a map, or that cannot be compared, are skipped, with the reason above them.
# kernel/process: the ELF loader disagrees with Linux on unusual program headers
SKIP="ExecTest.ElfWithNoLoadSegments:ExecTest.ElfWithUnalignedLoadOffset"
# kernel/process: PI operations are ENOSYS
SKIP="$SKIP:FutexTest.LockPi*"
# kernel/vfs: splice into a file stops quietly at RLIMIT_FSIZE
SKIP="$SKIP:SetRLimitTest.SpliceToRegularFileTruncatedByFSize"
SKIP="$SKIP:SetRLimitTest.SpliceToRegularFileWithExplicitOffset"
# kernel/procfs: absent per-process files (attr/, clear_refs, io, oom_adj, oom_score, sched,
# schedstat, timerslack_ns, wchan)
SKIP="$SKIP:ZombieProcTest.Attr*:ZombieProcTest.ClearRefs:ZombieProcTest.Io:ZombieProcTest.OomAdj"
SKIP="$SKIP:ZombieProcTest.OomScore:ZombieProcTest.Sched:ZombieProcTest.Schedstat"
SKIP="$SKIP:ZombieProcTest.TimerslackNs:ZombieProcTest.Wchan"
# Cannot be compared: the native kernel fails reading a zombie leader's auxv and environ, where the
# test (and Appine) expect them empty
SKIP="$SKIP:ZombieProcTest.Auxv:ZombieProcTest.Environ"
# Hang on both sides (killed after 60 s, so only the timeout would be compared): a thread ending
# through the raw exit syscall never marks itself exited, which musl's pthread_join waits for
# (glibc waits for the kernel's tid clearing); the robust futex test execs
# syscall_test_exec_child, a helper scripts/build-starnix-tests does not build, and then waits for
# it forever
SKIP="$SKIP:ExitTest.SysExitGroupMultipleThread:RobustFutexTest.FutexStateAfterExecCheck"
# Cannot be compared: docker sets its own RLIMIT_NOFILE; the test needs Linux 6.3 and skips itself
# on Appine's 6.1; the alarm timer is checked 5 µs after it is due, too close under load
SKIP="$SKIP:KernelDefaults/RLimitDefaultTest.CurAndMaxDefaults/NOFILE"
SKIP="$SKIP:PrctlTest.SetresuidAndSetresgidNoopPreservesFsIds:Timers.RealtimeAlarm"
sh /tmp/difftest-files/gtest/run.sh starnix "$SKIP" exec_test exit_test abort_test zombie_test \
    thread_group_lifecycle_test waitpid_flags_test waitpid_exit_signal_test pidfd_test kcmp_test \
    futex_test sched_test rlimit_test prctl_test sysinfo_test timers_test restart_read_test \
    sw_breakpoint_test test_helper_test trivial_test
