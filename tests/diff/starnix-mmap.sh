# docker-run-options: --privileged
# Fuchsia Starnix's syscall tests (mmap_test): mmap and mremap corner cases, MAP_GROWSDOWN, fork with PROT_NONE and copy-on-write, smaps/maps. Built by scripts/build-starnix-tests and
# installed by scripts/difftest at /usr/lib/starnix-tests; each test runs in its own process (one
# crash does not take the rest with it) under a 60 s timeout, and only its outcome is printed.
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
# Recorded in the kernel/mm map:
# - MAP_GROWSDOWN: an address hint in the guard gap is taken, kernel accesses below the mapping do
#   not grow it, a read below it after mprotect does not fault, VmStk is not kept
SKIP="MapGrowsdownTest.*"
# - mprotect accepts PROT_GROWSDOWN on a mapping that does not grow down (Linux: EINVAL)
SKIP="$SKIP:Mprotect.ProtGrowsdownOnNonGrowsdownMapping"
# - VMAs never merge (kernel/mm D7): /proc/<pid>/maps shows the pieces, and mremap with
#   MREMAP_FIXED or MREMAP_DONTUNMAP accepts a range spanning several mappings, which Linux before
#   6.17 refuses with EFAULT
SKIP="$SKIP:MMapProcTest.PrivateFileMappingCowModifiedPreservedAcrossFork"
SKIP="$SKIP:MMapProcTest.PrivateFileMappingMremapMovePreservesCowModified"
SKIP="$SKIP:Mremap.RemapMayMoveSpanningMappings:Mremap.MremapDontUnmapTwoSharedAnon"
# - smaps counts Rss of PROT_NONE and copy-on-write pages differently around fork
SKIP="$SKIP:MMapProcTest.CommittedProtNoneFileCowMemoryOnFork"
SKIP="$SKIP:MMapProcTest.CommittedProtNonePrivateMemoryOnFork"
SKIP="$SKIP:MMapProcTest.UncommittedProtNoneAndAccessibleMappingsOnFork"
# - mprotect changes nothing when a later VMA refuses (Linux has changed the earlier ones)
SKIP="$SKIP:MMapProcTest.MProtectAppliedPartially"
# - /proc/<pid>/maps prints a newline in a file name as is (Linux escapes it as \012)
SKIP="$SKIP:MMapProcTest.MapFileWithNewlineInName"
# - MAP_FIXED below mmap_min_addr (0x10000) is accepted
SKIP="$SKIP:MmapTest.MmapMinAddr"
# Fail on the native side, depending on its newer kernel (VMA merging of file mappings)
SKIP="$SKIP:MMapProcTest.AdjacentFileMappings"
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
