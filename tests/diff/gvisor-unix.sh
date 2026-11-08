# docker-run-options: --privileged
# difftest-files: gtest
# gVisor's syscall tests on AF_UNIX sockets: stream socket pairs of every kind (socketpair, path
# and abstract names, blocking or not), and bind, listen, accept and connect with both kinds of
# name. Run by tests/diff/gtest/run.sh, one process per test. Tests that differ for a reason
# recorded in a map, or that cannot be compared, are skipped, with the reason above them.
# kernel/unix: buffer sizes are only recorded (a send buffer does not limit what is queued)
SKIP="AllUnixDomainSockets/StreamUnixSocketPairTest.SendBufferOverflow/*"
SKIP="$SKIP:AllUnixDomainSockets/StreamUnixSocketPairTest.IncreasedSocketSendBufUnblocksWrites/*"
# kernel/unix: no ECONNRESET
SKIP="$SKIP:AllUnixDomainSockets/StreamUnixSocketPairTest.ReadOneSideClosedWithUnreadData/*"
# kernel/unix: connections carry no sender address; abstract names ignore the socket type
# (variants 4 to 7 are the abstract ones)
SKIP="$SKIP:AllUnixDomainSockets/AllSocketPairTest.BoundSenderAddr/*"
SKIP="$SKIP:AllUnixDomainSockets/AllSocketPairTest.BindAfterConnectSenderAddr/*"
SKIP="$SKIP:AllUnixDomainSockets/AllSocketPairTest.BindAfterAcceptSenderAddr/*"
SKIP="$SKIP:AllUnixDomainSockets/AllSocketPairTest.ConnectWithWrongType/4"
SKIP="$SKIP:AllUnixDomainSockets/AllSocketPairTest.ConnectWithWrongType/5"
SKIP="$SKIP:AllUnixDomainSockets/AllSocketPairTest.ConnectWithWrongType/6"
SKIP="$SKIP:AllUnixDomainSockets/AllSocketPairTest.ConnectWithWrongType/7"
sh /tmp/difftest-files/gtest/run.sh gvisor "$SKIP" socket_unix_stream_test accept_bind_test
