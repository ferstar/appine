# docker-run-options: --privileged
# difftest-files: gtest
# gVisor's syscall tests on processes and signals: fork, vfork, execve, exit, wait and waitid,
# pidfd, kill, tgkill, sigaction, sigprocmask, sigaltstack, sigtimedwait, group stops, signalfd,
# queued real-time signals, alarm, ptrace and futexes. Run by tests/diff/gtest/run.sh, one process
# per test. Tests that differ for a reason recorded in a map, or that cannot be compared, are
# skipped, with the reason above them.
# kernel/signal D5: no Yama (ptrace_scope reads 0, so these skip themselves on Appine)
P=PtraceTest
SKIP="$P.PrctlSetPtracer:$P.PrctlSetPtracerPersistsPastTraceeThreadExit"
SKIP="$SKIP:$P.PrctlSetPtracerPersistsPastLeaderExec:$P.PrctlSetPtracerDoesNotPersistPastNonLeaderExec"
SKIP="$SKIP:$P.PrctlSetPtracerDoesNotPersistPastTracerThreadExit"
SKIP="$SKIP:$P.PrctlSetPtracerRespectsTracerThreadID:$P.PrctlClearPtracerDoesNotAffectCurrentTracer"
SKIP="$SKIP:$P.PrctlNotInherited:$P.TraceParentNotAllowed:$P.TraceNonDescendantNotAllowed"
SKIP="$SKIP:$P.ExecvePtraceLockStress"
# kernel/signal: threads killed by another thread's execve report no PTRACE_EVENT_EXIT; detaching
# at an attach-time group stop reports the stop again
SKIP="$SKIP:TraceExec/PtraceExecveTest.Execve_GetRegs_PeekUser_SIGKILL_TraceClone_TraceExit/*"
SKIP="$SKIP:$P.AttachingConvertsGroupStopToPtraceStop"
# kernel/cred D2: no ACLs
SKIP="$SKIP:ExecTest.ACLExecuteOnlyBinary"
# kernel/signal: rt_sigqueueinfo to a zombie is ESRCH
SKIP="$SKIP:RtSignalTest.InvalidCodes"
# Hang on both sides until the 60 s timeout (only the timeout would be compared)
SKIP="$SKIP:SigtimedwaitTest.NullTimeoutReturnsEINTR:PidfdTest.SendSignalToStoppedChildSucceeds"
# Slow by design (they sleep 10 to 30 s each to show that nothing happens), left out for the time
# they would add to every run
SKIP="$SKIP:SigstopTest.Correctness:SigstopTest.RestartSyscall:SigtimedwaitTest.AlarmReturnsAlarm"
SKIP="$SKIP:AlarmTest.Restart:AlarmTest.UserModeSpinning:AlarmTest.SaSiginfo:AlarmTest.SaInterrupt"
SKIP="$SKIP:AlarmTest.Interrupt:VforkTest.ParentStopsUntilChildExecves"
SKIP="$SKIP:VforkTest.ExecedChildExitDoesntUnstopParent:VforkTest.SigchldReceivedDuringVforkIsDelivered"
SKIP="$SKIP:VforkTest.ParentStopsUntilChildExits:ExitTest.CloseFds"
sh /tmp/difftest-files/gtest/run.sh gvisor "$SKIP" fork_test vfork_test exec_test exit_test \
    wait_test pidfd_test kill_test tgkill_test sigaction_test sigprocmask_test sigaltstack_test \
    sigtimedwait_test sigstop_test signalfd_test rtsignal_test alarm_test ptrace_test futex_test
