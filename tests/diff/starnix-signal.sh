# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's syscall tests on signals, the vDSO, credentials, seccomp and the kernel log:
# signalfd, the vDSO and vvar mappings, seccomp filters, capabilities and file capabilities,
# set-id execution, syslog(2), /dev/kmsg and /proc/kmsg. Run by tests/diff/gtest/run.sh, one
# process per test. Tests that differ for a reason recorded in a map, or that cannot be compared,
# are skipped, with the reason above them.
# kernel/signal D4: there is no vvar page, and the vDSO is a private anonymous copy
SKIP="VdsoProcTest.*"
# kernel/signal: MADV_DONTNEED wipes the vDSO (the next call through it is SIGILL)
SKIP="$SKIP:VdsoTest.VdsoCanBeMadvised"
# kernel/cred: the vDSO bypass covers every clock; capset does not mask bits above CAP_LAST_CAP
SKIP="$SKIP:SeccompTest.NonVdsoClockGettimeFiltered:CapsTest.CapsetMasksReservedBits"
# kernel/signal: coalesced standard signals report the last sender (the forced SIGSYS replaces the
# pending SI_TKILL one)
SKIP="$SKIP:SeccompTest.RetTrapBlockedUnmasksAndResetsHandlerAtSendTime"
# kernel/vfs: security.capability writes check the magic under a mask and accept any v2 or v3
# size, so malformed values are stored
SKIP="$SKIP:FileCapTest.MalformedAndRevision1FileCapabilitiesRejected"
# kernel/procfs: read-only proc files cannot be opened for writing even with CAP_DAC_OVERRIDE
SKIP="$SKIP:SyslogAccessTestAll/SyslogAccessTest.ProcKmsgWrite/*DacOverride*"
SKIP="$SKIP:SyslogAccessTestAll/SyslogAccessTest.ProcKmsgTrunc/*DacOverride*"
SKIP="$SKIP:SyslogAccessTestAll/SyslogAccessTest.ProcKmsgReadWrite/*DacOverride*"
# Fail on the native side at times: docker's own messages keep the VM's kernel log wrapping, so a
# read from its start can find the record gone
SKIP="$SKIP:SyslogTest.Read:SyslogTest.ReadDevKmsg:SyslogTest.DevKmsgSeekSet:SyslogTest.DevKmsgSeekEnd"
sh /tmp/difftest-files/gtest/run.sh starnix "$SKIP" signalfd_test vdso_test seccomp_test \
    capabilities_test suid_test syslog_test
