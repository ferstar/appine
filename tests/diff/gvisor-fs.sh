# docker-run-options: --privileged
# difftest-files: gtest
# gVisor's syscall tests on files, terminals and mounts: PTYs and job control, pivot_root and
# chroot, inotify, fcntl and flock, open and openat2, rename, getdents, close_range, chown and
# pipes. Run by tests/diff/gtest/run.sh, one process per test. Tests that differ for a reason
# recorded in a map, or that cannot be compared, are skipped, with the reason above them.
# kernel/tty: TIOCSPGRP error codes; kernel/procfs fidelity gaps: the devpts root is 0555
SKIP="JobControlTest.SetForegroundProcessGroupEmptyProcessGroup:SetModeTest.SetMode/*"
# kernel/vfs D6: mount propagation is not emulated; kernel/vfs: mounts copied into a less
# privileged namespace are not locked
SKIP="$SKIP:PivotRootTest.OnSharedNewRoot:PivotRootTest.OnSharedNewRootParent"
SKIP="$SKIP:PivotRootTest.OnSharedPutOldMountpoint:PivotRootTest.LockedNewRootFails"
# kernel/vfs: utimensat always reports IN_ATTRIB; watches on synthetic nodes never report;
# IN_EXCL_UNLINK filters the wrong events; executing a file reports no inotify events
SKIP="$SKIP:Inotify.Utimensat:Inotify.KernfsBasic:Inotify.KernfsDynamicFileAcrossSave"
SKIP="$SKIP:Inotify.ExcludeUnlink*:Inotify.Exec"
# kernel/vfs: F_GETFL never shows O_DIRECT; open with O_PATH keeps other status flags; locks on
# inode-less files (pipes) are no-ops
SKIP="$SKIP:FcntlTest.GetAllFlags:FcntlTest.GetOpathFlag"
SKIP="$SKIP:FlockTestNoFixture.FlockSymlink:FlockTestNoFixture.FlockPipe"
# kernel/vfs: O_TRUNC with O_RDONLY is only half a write; access mode 3 opens read-write;
# renameat2 ignores RENAME_NOREPLACE for . and ..; rename in synthetic filesystems is EACCES first
SKIP="$SKIP:OpenTest.OTrunc:OpenTest.OTruncAndReadOnlyDir:OpenTest.CanTruncateReadOnly"
SKIP="$SKIP:OpenTest.OpenWithStrangeFlags:Renameat2Test.NoReplaceDot"
SKIP="$SKIP:RenameTest.SysfsFileToSelf:RenameTest.SysfsDirectoryToSelf"
SKIP="$SKIP:RenameTest.SysfsPathEndingWithDots"
# kernel/vfs: SEEK_END on directories is EINVAL
SKIP="$SKIP:GetdentsTest/0.Issue128ProcSeekEnd"
# kernel/vfs: scoped openat2 never returns EAGAIN (the rename variants loop for minutes)
SKIP="$SKIP:Openat2Test.BeneathRaceWithMount:Openat2Test.InRootRaceWithMount"
SKIP="$SKIP:Openat2Test.BeneathRaceWithRename:Openat2Test.InRootRaceWithRename"
# kernel/cred: per-thread id changes leak to every thread (a helper thread's setresuid takes the
# main thread's CAP_CHOWN); kernel/vfs: anonymous pipes and sockets cannot change owner
SKIP="$SKIP:ChownKinds/ChownParamTest.ChownFileSucceedsAsRoot/*"
SKIP="$SKIP:ChownTest.FchownPipeFileSucceeds:ChownTest.FchownPipeFileFails"
# mount_test. kernel/vfs D6: mount propagation is not emulated (no peer groups, no shared:N in
# mountinfo, no events)
M=MountTest
SKIP="$SKIP:$M.ArgumentsAreIgnored:$M.BindParentToChild:$M.BindPeerGroupsWithChildren"
SKIP="$SKIP:$M.BindSharedOnShared:$M.BindSlaveToShared:$M.InerheritPropagation"
SKIP="$SKIP:$M.LargeTreePropagationEvent:$M.MakeMultipleShared:$M.MakePeer:$M.MakeShared"
SKIP="$SKIP:$M.MakeSharedSlave:$M.MakeSlave:$M.MountMoveSharedParent:$M.MountNamespacePropagation"
SKIP="$SKIP:$M.MountNamespaceSlavesNewUserNamespace:$M.PrivateMasterUnslaves:$M.PropagateMountEvent"
SKIP="$SKIP:$M.RecursiveBindPropagation:$M.SetMountPropagationOfStackedMounts"
SKIP="$SKIP:$M.SetPropagationRecursive:$M.SlaveMaster:$M.UmountIgnoresPeersWithChildren"
SKIP="$SKIP:$M.UmountPropagatedSubtreeFromPrivilegedNS:$M.UmountReparentsCoveredMounts"
SKIP="$SKIP:$M.MaxMounts:$M.MaxMountsWithSlave:$M.FailedBindDoesNotBreakPropagation"
# No overlayfs (kernel/vfs: other filesystem types are ENODEV); sysfs has no /sys/kernel
# (kernel/procfs)
SKIP="$SKIP:$M.LockedMountStopsOverlayLowerdir:$M.LockedMountStopsOverlayUpperdir"
SKIP="$SKIP:$M.OverlayRefusesLayerFromAnotherMountNamespace:$M.OverlayfsDirectoryRenameInUserNamespace"
SKIP="$SKIP:$M.OverlayfsRejectsEmptyOptions:$M.OverlayfsRelativePaths"
SKIP="$SKIP:$M.OverlayfsRenameFromDeletedDirFails:$M.OverlayfsRenameOverWhiteoutInStickyDir"
SKIP="$SKIP:$M.OverlayfsRenameWhiteoutOnlyWhenLowerExists"
SKIP="$SKIP:$M.OverlayfsSecurityCapabilityRequiresSetFcap"
SKIP="$SKIP:$M.OverlayfsSecurityCapabilityTranslatesRootID:$M.OverlayfsSgidBitIsCopiedUp"
SKIP="$SKIP:$M.OverlayfsUnlinkInDeletedDirFails:$M.LockedMountStopsOverlayOfSysfs"
SKIP="$SKIP:$M.UserNamespaceSysfsMountRevealingLockedMount"
# kernel/vfs: mounts copied into a less privileged namespace are not locked (flags, submounts);
# proc and sysfs mounts in a user namespace skip Linux's owner and visibility checks
SKIP="$SKIP:$M.ChangeMountFlagsOfLockedMount:$M.LockedMountFlagsAreInheritedByBind"
SKIP="$SKIP:$M.LockedMountStopsNonRecBind:$M.UserNamespaceProcMountRevealingLockedMount"
SKIP="$SKIP:$M.UserNamespaceProcMountLockedReadOnly:$M.UserNamespaceProcBindMountIsNotVisible"
# kernel/vfs D7: fsopen and fsmount are ENOSYS (these skip themselves on Appine); kernel/procfs: no
# change notifications (mountinfo cannot be polled); kernel/vfs: atime ignores the guest's settings;
# kernel/vfs D3: tmpfs counts file sizes (no holes, no SIGBUS when full); kernel/cred D1: one
# credential per thread group (a helper thread drops CAP_SYS_ADMIN for the cleanup too)
SKIP="$SKIP:$M.UserNamespaceProcFsmount*:$M.PollMountInfo:$M.MountInfoAfterUnshare"
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
