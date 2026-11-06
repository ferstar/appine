# docker-run-options: --privileged
# Fuchsia Starnix's syscall tests (waitpid_flags_test waitpid_exit_signal_test): waitpid flags (__WCLONE, __WALL) against children's exit signals. Built by scripts/build-starnix-tests and
# installed by scripts/difftest at /usr/lib/starnix-tests; each test runs in its own process (one
# crash does not take the rest with it) under a 60 s timeout (SIGKILL), and only its outcome is
# printed.
# The shell's own crash reports are filtered out (where busybox prints them varies). Tests in SKIP
# differ from Linux for a recorded reason (see the comments) and are not run.
T=/usr/lib/starnix-tests
if [ ! -d $T ]; then
    echo "Starnix tests not installed: run scripts/build-starnix-tests, then scripts/difftest"
    exit 0
fi
cd /tmp
# Core files would change the shell's report of a crash on one side only
ulimit -c 0
# Recorded in the kernel/process map: wait ignores __WCLONE and __WALL and the children's exit
# signals, so any child is reaped under any flags
SKIP="WaitpidFlagsTest.waitpidWithCloneFlagFailsWhenChildSendsSIGCHLD"
SKIP="$SKIP:WaitpidFlagsTest.waitpidWithNoFlagsFailsWhenChildSendsNoSignal"
SKIP="$SKIP:WaitpidFlagsTest.waitpidWithNoFlagsFailsWhenChildSendsSIGUSR1"
for bin in waitpid_flags_test waitpid_exit_signal_test; do
    $T/bin/$bin --gtest_list_tests --gtest_filter="-$SKIP" |
        awk '/^[^ ]/ { s = $1 } /^  / { print s $1 }' | while read -r t; do
        # SIGKILL, and the output in a file: a test that ignores SIGTERM, or a child of it that
        # keeps the output open, would otherwise hold the case (and its difftest lane) forever
        timeout -s KILL 60 $T/bin/$bin --gtest_filter="$t" --gtest_print_time=0 >starnix.out 2>&1
        code=$?
        out=$(cat starnix.out)
        case $out in
        *"[       OK ] $t"*) r=ok ;;
        *"[  SKIPPED ] $t"*) r=skipped ;;
        *"[  FAILED  ] $t"*) r=failed ;;
        *) r="exit $code" ;;
        esac
        echo "$t $r"
    done
done 2>&1 | grep -v -E '^(Segmentation fault|Aborted|Bus error|Illegal instruction|Killed)'
rm -f starnix.out
