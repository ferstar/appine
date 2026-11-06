# docker-run-options: --privileged
# Fuchsia Starnix's syscall tests (mmap_test): mmap and mremap corner cases, MAP_GROWSDOWN, fork with PROT_NONE and copy-on-write, smaps/maps. Built by scripts/build-starnix-tests and
# installed by scripts/difftest at /usr/lib/starnix-tests; each test runs in its own process (one
# crash does not take the rest with it) under a 60 s timeout, and only its outcome is printed.
# The shell's own crash reports are filtered out (where busybox prints them varies). Tests in SKIP
# are not run, for the reason in the comment above them.
T=/usr/lib/starnix-tests
if [ ! -d $T ]; then
    echo "Starnix tests not installed: run scripts/build-starnix-tests, then scripts/difftest"
    exit 0
fi
cd /tmp
# Core files would change the shell's report of a crash on one side only
ulimit -c 0
# Fail on the native side, depending on its newer kernel (VMA merging of file mappings)
SKIP="MMapProcTest.AdjacentFileMappings"
SKIP="$SKIP:MMapProcTest.PrivateFileMappingMremapShrinkAndMmapRetainsSeparateMappings"
for bin in mmap_test; do
    $T/bin/$bin --gtest_list_tests --gtest_filter="-$SKIP" |
        awk '/^[^ ]/ { s = $1 } /^  / { print s $1 }' | while read -r t; do
        out=$(timeout 60 $T/bin/$bin --gtest_filter="$t" --gtest_print_time=0 2>&1)
        code=$?
        case $out in
        *"[       OK ] $t"*) r=ok ;;
        *"[  SKIPPED ] $t"*) r=skipped ;;
        *"[  FAILED  ] $t"*) r=failed ;;
        *) r="exit $code" ;;
        esac
        echo "$t $r"
    done
done 2>&1 | grep -v -E '^(Segmentation fault|Aborted|Bus error|Illegal instruction|Killed)'
