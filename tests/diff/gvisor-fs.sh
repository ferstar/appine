# docker-run-options: --privileged
# difftest-files: gtest
# gVisor's syscall tests on files, terminals and mounts: PTYs and job control, pivot_root and
# chroot, inotify, fcntl and flock, open and openat2, rename, getdents, close_range, chown and
# pipes. Run by tests/diff/gtest/run.sh, one process per test. Tests that differ for a reason
# recorded in a map, or that cannot be compared, are skipped, with the reason above them.
# kernel/inotify: procfs opens and reads do not emit inotify events
SKIP="Inotify.KernfsBasic:Inotify.KernfsDynamicFileAcrossSave"
# kernel/vfs: scoped openat2 never returns EAGAIN (the rename variants loop for minutes)
SKIP="$SKIP:Openat2Test.BeneathRaceWithMount:Openat2Test.InRootRaceWithMount"
SKIP="$SKIP:Openat2Test.BeneathRaceWithRename:Openat2Test.InRootRaceWithRename"
M=MountTest
# kernel/sysfs: sysfs has no /sys/kernel
SKIP="$SKIP:$M.LockedMountStopsOverlayOfSysfs"
SKIP="$SKIP:$M.UserNamespaceSysfsMountRevealingLockedMount"
# kernel/vfs D7: fsopen and fsmount are ENOSYS (these skip themselves on Appine)
SKIP="$SKIP:$M.UserNamespaceProcFsmount*"
# This test sleeps 30 s to check that shared locks do not block. It runs in gvisor-slow-process.
SKIP="$SKIP:FlockTest.BlockingLockNoBlockingForSharedLocks"
# Cannot be compared: Linux 6.4 made O_CREAT|O_DIRECTORY EINVAL, 6.1 (and Appine) answer ENOTDIR.
# The directory link-count test runs on tmpfs in gvisor-syscalls.
SKIP="$SKIP:Openat2Test.DirectoryWithCreate:RenameTest.DirectoryOverwritesDirectoryLinkCount"
sh /tmp/difftest-files/gtest/run.sh gvisor "$SKIP" pty_test pivot_root_test chroot_test \
    mount_test inotify_test fcntl_test flock_test open_test openat2_test rename_test getdents_test \
    close_range_test chown_test pipe_test
