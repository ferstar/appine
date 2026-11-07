# Runs Fuchsia Starnix's syscall tests for the starnix-* cases:
#
#   sh run.sh <skip> <program>...
#
# The programs are built by scripts/build-starnix-tests and installed by scripts/difftest at
# /usr/lib/starnix-tests. Each test runs in its own process (one crash does not take the rest with
# it) under a 60 s timeout (SIGKILL), and only "<test> ok|failed|skipped|exit <code>" is printed.
# <skip> is a gtest filter of tests not to run, ':'-separated (empty for none); the cases keep the
# reason beside each name. Without the installed tests the case prints that they are missing.
#
# Built programs in no case, and why:
# - they act on the development VM itself with the native side's privileges: reboot_test,
#   sysfs_power_test (suspends), time_test and timerfd_mutable_test (set the clock);
# - Appine does not implement what they test: bpf_test (bpf(2) has only cgroup device programs,
#   which the bpf-devices case covers), perf_event_open_test, process_mrelease_test and
#   userfaultfd_test (ENOSYS), overlayfs_test (no overlayfs; it fails natively too, docker's root
#   being an overlay), tun_test;
# - devices neither side has: input_test, uinput_test, udev_test, touch_standby_test,
#   custom_artifact_test, ext4_write_test;
# - nothing to compare: cache_flush_test has no tests on arm64, and uio_test's one test is
#   process_vm_readv's recorded deviation (the kernel/signal map).
T=/usr/lib/starnix-tests
if [ ! -d $T ]; then
    echo "Starnix tests not installed: run scripts/build-starnix-tests, then scripts/difftest"
    exit 0
fi
skip=$1
shift
cd /tmp
# Core files would change the shell's report of a crash on one side only
ulimit -c 0
for bin in "$@"; do
    # Disabled tests do not run even when named in a filter
    $T/bin/$bin --gtest_list_tests --gtest_filter="-$skip" |
        awk '/^[^ ]/ { s = $1 } /^  / { print s $1 }' | grep -v DISABLED_ | while read -r t; do
        # SIGKILL, and the output in a file: a test that ignores SIGTERM, or a child of it that
        # keeps the output open, would otherwise hold the case (and its difftest lane) forever.
        # stdin is not the loop's list of tests, which a test reading its terminal would eat
        timeout -s KILL 60 $T/bin/$bin --gtest_filter="$t" --gtest_print_time=0 \
            </dev/null >starnix.out 2>&1
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
# The shell's own crash reports are filtered out (where busybox prints them varies)
done 2>&1 | grep -v -E '^(Segmentation fault|Aborted|Bus error|Illegal instruction|Killed)'
rm -f starnix.out
