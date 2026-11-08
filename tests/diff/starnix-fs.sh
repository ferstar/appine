# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's syscall tests on files, pipes and mounts: file I/O and metadata, fcntl locks
# and flags, fallocate, renameat2, symlinks, xattrs, tmpfs, sync, sendfile, splice and vmsplice,
# pipes, AIO, inotify, mount and its flags, loop devices. Run by tests/diff/gtest/run.sh, one
# process per test. Tests that differ for a reason recorded in a map, or that cannot be compared,
# are skipped, with the reason above them.
# kernel/vfs: AIO contexts belong to the thread group, not the address space
SKIP="AioWriteTest.WriteCompletesAfterSubmittingProcessExits"
# kernel/vfs: F_SETFL takes O_NOATIME with no ownership check; getdents64 copies all or nothing;
# no inode flags (FS_IOC_SETFLAGS is ENOTTY); no O_TMPFILE, and open(2) does not validate it
SKIP="$SKIP:FcntlTest.NoatimePermission:FsTest.GetdentsLazyAndUnmappedBuffer"
SKIP="$SKIP:TmpfsTest.CasefoldDisabledByDefault"
SKIP="$SKIP:OtmpfileTest.TmpFileFailWithRdOnlyAccessMode:OtmpfileTest.TmpFileWithOCreatShouldFail"
# kernel/procfs: /sys/block/loop<n> entries are directories, not links into /sys/devices
SKIP="$SKIP:LoopTest.SysfsDeviceDirectoryRemovalAndReplacement"
# kernel/vfs: propagation is not emulated, and propagation changes on mounts outside the caller's
# namespace (a lazily unmounted one) are EINVAL
SKIP="$SKIP:MountTest.BasicSharing:MountTest.InheritShared:MountTest.PropagateOntoMountRoot"
SKIP="$SKIP:MountTest.PropagateOvermount:MountTest.PropagateUnmount:MountTest.QuizAMountMove"
SKIP="$SKIP:MountTest.QuizCPropagation:DetachedMountTest.ChangePropagationSucceeds"
# kernel/vfs: umount ignores mappings; pivot_root resolves both paths before checking them;
# mountinfo loses the root of a bind whose source was removed
SKIP="$SKIP:MountTest.BusyWithMmap:MountTest.PivotRootErrors:MountTest.ProcMountInfoRoot"
# kernel/vfs: a filesystem has no superblock flags of its own (a tmpfs mounted read-only becomes
# writable through a bind remount, and mountinfo's super options are always rw)
SKIP="$SKIP:MountTest.RemountReadOnlyToReadWriteIgnored:MountTest.RemountBindReadOnlyFlagInheritance"
SKIP="$SKIP:ProcMountsTest.RemountBindReadOnlyFlagInheritance"
SKIP="$SKIP:MountTest/ProcMountinfoSuperblockFlagsTest.FlagReporting/MS_DIRSYNC"
SKIP="$SKIP:MountTest/ProcMountinfoSuperblockFlagsTest.FlagReporting/MS_LAZYTIME"
SKIP="$SKIP:MountTest/ProcMountinfoSuperblockFlagsTest.FlagReporting/MS_MANDLOCK"
SKIP="$SKIP:MountTest/ProcMountinfoSuperblockFlagsTest.FlagReporting/MS_RDONLY"
SKIP="$SKIP:MountTest/ProcMountinfoSuperblockFlagsTest.FlagReporting/MS_SYNCHRONOUS"
# kernel/procfs: procfs mount options (hidepid, gid) are ignored
SKIP="$SKIP:ProcMountsTest.FilesystemMountOptions"
# kernel/vfs: O_NOTIFICATION_PIPE is EINVAL; pipe I/O with faulting buffers ignores Linux's
# per-buffer rules; splice-family argument checks differ; splice into a file stops quietly at
# RLIMIT_FSIZE; vmsplice copies the data at the call
SKIP="$SKIP:PipeTest.NotificationPipeUnsupported:PipeTest.PartialEfaultRollbackAndShortCounts"
SKIP="$SKIP:PipeTest.SpliceValidationAndRlimitFsize"
SKIP="$SKIP:VmspliceTest.FileInPipe:VmspliceTest.ForkWithBufferInPipe:VmspliceTest.ModifyBufferInPipe"
SKIP="$SKIP:VmspliceTest.RemapSharedBufferInPipeThenModify:VmspliceTest.UnmapBufferInPipe"
SKIP="$SKIP:VmspliceTest.UnmapBufferInPipeThenMapInPlace"
SKIP="$SKIP:VmspliceTest/VmspliceUnmapBufferAfterVmsplicingRemappedBufferTest.*"
# kernel/poll: pipe POLLOUT counts bytes, not buffers
SKIP="$SKIP:PipeTest.PolloutFragmentation"
# Fail on the native side, which runs in docker: its pid namespace turns a dead lock owner's pid
# into 0; its device cgroup refuses mknod of a character device; its newer kernel answers ENOENT
# where the test expects EINVAL for operations on a lazily unmounted mount
SKIP="$SKIP:FcntlLockTest.CloneFiles:MknodTest.MknodChrZeroDoesNotRequireCapMknod"
SKIP="$SKIP:DetachedMountTest.BindFails:DetachedMountTest.MoveFails:DetachedMountTest.PivotRootFails"
SKIP="$SKIP:DetachedMountTest.SubmountFails:DetachedMountTest.UnsharePreservesDetachedState"
sh /tmp/difftest-files/gtest/run.sh starnix "$SKIP" fs_test fcntl_test fallocate_test read_write_test \
    renameat2_exchange_sticky_test symlink_test xattr_test tmpfs_test sync_test sendfile_test \
    vmsplice_test pipe_test aio_test inotify_test mount_test loop_test
