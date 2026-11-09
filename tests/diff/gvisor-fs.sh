# docker-run-options: --privileged
# difftest-files: gtest
# gVisor's syscall tests on files, terminals and mounts: PTYs and job control, pivot_root and
# chroot, inotify, fcntl and flock, open and openat2, rename, getdents, close_range, chown and
# pipes. Run by tests/diff/gtest/run.sh, one process per test. Tests that differ for a reason
# recorded in a map, or that cannot be compared, are skipped, with the reason above them.
# kernel/tty: TIOCSPGRP error codes; kernel/procfs fidelity gaps: the devpts root is 0555
SKIP="JobControlTest.SetForegroundProcessGroupEmptyProcessGroup:SetModeTest.SetMode/*"
# kernel/vfs: mounts copied into a less privileged namespace are not locked
SKIP="$SKIP:PivotRootTest.LockedNewRootFails"
# kernel/vfs: utimensat always reports IN_ATTRIB; watches on synthetic nodes other than cgroup2's
# never report (procfs opens and reads);
# IN_EXCL_UNLINK filters the wrong events; executing a file reports no inotify events
SKIP="$SKIP:Inotify.Utimensat:Inotify.KernfsBasic:Inotify.KernfsDynamicFileAcrossSave"
SKIP="$SKIP:Inotify.ExcludeUnlink*:Inotify.Exec"
# kernel/vfs: F_GETFL never shows O_DIRECT; open with O_PATH keeps other status flags; locks on
# inode-less files (pipes) are no-ops
SKIP="$SKIP:FcntlTest.GetAllFlags:FcntlTest.GetOpathFlag"
SKIP="$SKIP:FlockTestNoFixture.FlockSymlink:FlockTestNoFixture.FlockPipe"
# kernel/vfs: O_TRUNC with O_RDONLY is only half a write; access mode 3 opens read-write
SKIP="$SKIP:OpenTest.OTrunc:OpenTest.OTruncAndReadOnlyDir:OpenTest.CanTruncateReadOnly"
SKIP="$SKIP:OpenTest.OpenWithStrangeFlags"
# kernel/vfs: scoped openat2 never returns EAGAIN (the rename variants loop for minutes)
SKIP="$SKIP:Openat2Test.BeneathRaceWithMount:Openat2Test.InRootRaceWithMount"
SKIP="$SKIP:Openat2Test.BeneathRaceWithRename:Openat2Test.InRootRaceWithRename"
# kernel/vfs: anonymous pipes and sockets cannot change owner
SKIP="$SKIP:ChownTest.FchownPipeFileSucceeds:ChownTest.FchownPipeFileFails"
M=MountTest
# mount_test. kernel/vfs: mount and unmount events do not propagate between peers and slaves (only
# the types are recorded and shown)
SKIP="$SKIP:$M.BindSharedOnShared:$M.BindSlaveToShared:$M.LargeTreePropagationEvent"
SKIP="$SKIP:$M.MountNamespacePropagation:$M.MountNamespaceSlavesNewUserNamespace"
SKIP="$SKIP:$M.PropagateMountEvent:$M.RecursiveBindPropagation:$M.UmountIgnoresPeersWithChildren"
SKIP="$SKIP:$M.UmountPropagatedSubtreeFromPrivilegedNS:$M.MaxMounts:$M.MaxMountsWithSlave"
SKIP="$SKIP:$M.FailedBindDoesNotBreakPropagation"
# kernel/procfs: sysfs has no /sys/kernel
SKIP="$SKIP:$M.LockedMountStopsOverlayOfSysfs"
SKIP="$SKIP:$M.UserNamespaceSysfsMountRevealingLockedMount"
# kernel/vfs: mounts copied into a less privileged namespace are not locked (flags, submounts);
# proc and sysfs mounts in a user namespace skip Linux's owner and visibility checks
SKIP="$SKIP:$M.ChangeMountFlagsOfLockedMount:$M.LockedMountFlagsAreInheritedByBind"
SKIP="$SKIP:$M.LockedMountStopsNonRecBind:$M.UserNamespaceProcMountRevealingLockedMount"
SKIP="$SKIP:$M.UserNamespaceProcMountLockedReadOnly:$M.UserNamespaceProcBindMountIsNotVisible"
# kernel/vfs D7: fsopen and fsmount are ENOSYS (these skip themselves on Appine); kernel/vfs: atime
# ignores the guest's settings;
# kernel/vfs D3: tmpfs counts file sizes (no holes, no SIGBUS when full); kernel/cred D1: one
# credential per thread group (a helper thread drops CAP_SYS_ADMIN for the cleanup too)
SKIP="$SKIP:$M.UserNamespaceProcFsmount*"
SKIP="$SKIP:$M.MountNoAtime:$M.TmpfsSizePartialWriteMultiplePages:$M.TmpfsSizeMmap"
SKIP="$SKIP:$M.UmountPermDenied"
# Slow by design (it sleeps 30 s to show that shared locks do not block), left out for the time it
# would add to every run
SKIP="$SKIP:FlockTest.BlockingLockNoBlockingForSharedLocks"
# Cannot be compared: Linux 6.4 made O_CREAT|O_DIRECTORY EINVAL, 6.1 (and Appine) answer ENOTDIR;
# docker's /tmp is an overlay, so the native side skips the link-count test
SKIP="$SKIP:Openat2Test.DirectoryWithCreate:RenameTest.DirectoryOverwritesDirectoryLinkCount"
sh /tmp/difftest-files/gtest/run.sh gvisor "$SKIP" pty_test pivot_root_test chroot_test \
    mount_test inotify_test fcntl_test flock_test open_test openat2_test rename_test getdents_test \
    close_range_test chown_test pipe_test
