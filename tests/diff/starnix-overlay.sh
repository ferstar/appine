# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's overlayfs tests: merged listings, whiteouts and opaque directories, unlink,
# rmdir, creating over removed names, rewriting, appending, links and renames through the
# overlay, and the mounter's and other users' access checks; then fs_test's permission, chmod,
# chown and open tests on an overlay. Their layers are on a tmpfs (TEST_TMPDIR): docker's root is
# itself an overlay, which Linux refuses as an upper layer. Run by tests/diff/gtest/run.sh, one
# process per test. The O_TMPFILE link and fd-name tests open /tmp directly.
# Cannot be compared on CI: its kernel defaults to redirect_dir=on, so a merged directory renames
# in place there, where 6.1's default (follow, as in Appine and on 6.8) refuses it with EXDEV
SKIP="OverlayFsTest.RenameDir"
set -e
# Keep the shipped runner reachable after mounting over /tmp.
cd /tmp/difftest-files
mount -t tmpfs tmpfs /tmp
export TEST_TMPDIR=/tmp
ONLY='OverlayFs*:OtmpfileTest.TmpFileLinkIntoAfter:OtmpfileTest.TmpFileWithOExclShouldFailLinkInto'
export ONLY="$ONLY:ProcSelfFdTest.TmpFileFdName:ProcSelfFdTest.TmpFileLinkIntoAfterFdName"
sh ./gtest/run.sh starnix "$SKIP" overlayfs_test fs_test procfs_test
# BusyBox timeout helpers can still have their working directory on this mount.
umount -l /tmp
