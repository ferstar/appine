# docker-run-options: --privileged
# difftest-files: starnix
# Fuchsia Starnix's syscall tests on memory: mmap and mremap corner cases, address-space bounds,
# anonymous inodes, mlock, memfd, membarrier, /proc/<pid>/mem and pagemap, personality. Run by
# tests/diff/starnix/run.sh, one process per test. Tests that differ for a reason recorded in a
# map, or that cannot be compared, are skipped, with the reason above them.
# Fail on the native side, depending on its newer kernel (VMA merging of file mappings)
SKIP="MMapProcTest.AdjacentFileMappings"
SKIP="$SKIP:MMapProcTest.PrivateFileMappingMremapShrinkAndMmapRetainsSeparateMappings"
# No ASLR (the kernel/process map's open question): the stack ends at the top of the address space,
# so the page the test maps there is taken and it skips itself
SKIP="$SKIP:HighestAddress/AspaceTest.RangeFault/*"
# bpf(2) has only cgroup device programs, so no map fd (kernel/procfs); userfaultfd stays ENOSYS
# (docs/starnix.md)
SKIP="$SKIP:All/AnonInodeTest.*/BpfMap:All/AnonInodeTest.*/Userfaultfd"
# kernel/mm: mlock never fails on inaccessible ranges; mlock population inflates RSS and does not
# break copy-on-write
SKIP="$SKIP:All/MlockProtTest.MlockApplied/None_*:MlockProcTest.MlockStopsPopulatingAtProtNoneMapping"
SKIP="$SKIP:MlockProcTest.MlockAfterForkCopiesPrivatePages"
SKIP="$SKIP:MlockProcTest.MlockPrefaultsOnlyWritablePrivateAnonymousMappings"
# kernel/vfs: MFD_NOEXEC_SEAL is accepted but not enforced
SKIP="$SKIP:MemfdTest.ChmodWithNOEXEC"
# kernel/mm: READ_IMPLIES_EXEC is ignored
SKIP="$SKIP:PersonalityTest.ReadImpliesExecMmap:PersonalityTest.ReadImpliesExecMmapIoUring"
SKIP="$SKIP:PersonalityTest.ReadImpliesExecMprotect:PersonalityTest.ReadImpliesExecMprotectAfterFork"
SKIP="$SKIP:PersonalityTest.ReadImpliesExecMprotectFile"
SKIP="$SKIP:PersonalityTest.ReadImpliesExecMprotectMixedMappings"
# kernel/procfs: the open-time ptrace check uses real credentials
SKIP="$SKIP:ProcTestBase.ProcMemAccessGatedByFsUidSymmetric"
# kernel/mm: pagemap reports no frame numbers, not even to CAP_SYS_ADMIN (after that check fails,
# the fallback test crashes in gtest's own allocation)
SKIP="$SKIP:ProcTestBase.ProcSelfPagemapMappedAndUnmapped:ProcTestBase.ProcSelfPagemapMultiChunkBoundary"
SKIP="$SKIP:ProcTestBase.ProcSelfPagemapMultipleMappingsAndHoles"
SKIP="$SKIP:ProcTestBase.ProcSelfPagemapFallbackBoundary"
sh /tmp/difftest-files/starnix/run.sh "$SKIP" mmap_test aspace_test anon_test mlock_test \
    memfd_test membarrier_test procfs_mem_test personality_test
