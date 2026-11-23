# difftest-files: gtest
# gVisor's tests for cross-process memory transfers and resizing or moving mappings.
# kernel/ptrace: an oversized remote iovec count is rejected before the empty local transfer.
SKIP="ProcessVMInvalidTest.EmptyTransferPrecedesRemoteValidation"
# kernel/mm: growing a shared anonymous mapping allocates fresh pages past the shrunken end.
SKIP="$SKIP:MremapDeathTest.SharedAnon"
sh /tmp/difftest-files/gtest/run.sh gvisor "$SKIP" process_vm_read_write_test mremap_test
