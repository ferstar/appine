# docker-run-options: --privileged
# difftest-files: gtest
# Fuchsia Starnix's syscall tests on readiness, timers, terminals and netlink: epoll, poll,
# timerfd, getrandom, ioctls on sockets and terminals, PTYs, rtnetlink. Run by
# tests/diff/gtest/run.sh, one process per test. Tests that differ for a reason recorded in a
# map, or that cannot be compared, are skipped, with the reason above them.
# kernel/poll: epoll_wait does not check the event buffer up front
SKIP="EpollTest.WaitInvalidParams"
# kernel/vfs: alarm timerfds need no capability
SKIP="$SKIP:TimerFD.NoWakeAlarmCap:TimerFD.NoWakeAlarmCapSetTime"
# kernel/tty: /dev/tty cannot be told apart from the slave it opens
SKIP="$SKIP:Pty.OpenDevTTY"
# kernel/net: host interfaces cannot be changed (interface setters are EPERM, before the name is
# looked up), and multicast groups never hear anything
SKIP="$SKIP:IoctlTest.SIOCSIFFLAGS_Success:IoctlInvalidTest/IoctlInvalidTest.InvalidRequest/9"
SKIP="$SKIP:RouteNetlinkSocket.AddDropMulticastGroup"
# kernel/net: netlink requests are not validated as Linux validates them; single RTM_GETNEIGH
# requests always answer ENOENT; unsupported rtnetlink change requests skip the capability check
SKIP="$SKIP:NetlinkRouteTest.IflaCacheInfoMissingBody:NetlinkRouteTest.IncompleteRouteFlow"
SKIP="$SKIP:NetlinkRouteTest.MalformedMessageCausesErrorReturn"
SKIP="$SKIP:RouteNetlinkSocket/RouteNetlinkSocketStrictOpt.MalformedDumpRequest/strict"
SKIP="$SKIP:RouteNetlinkSocket/RouteNetlinkSocketGetNeigh.GetNeighErrors/*"
SKIP="$SKIP:NetlinkRouteTest.NoCapabilities"
# No tun device (kernel/net D1), which these suites create their interface with
SKIP="$SKIP:RouteNetlinkSocket/RouteNetlinkSocketNewAddr.AddSubnetRoute/*"
SKIP="$SKIP:RouteNetlinkSocketTestWithInterface.*"
# Cannot be compared: the native side has no input devices; there SIOCSIFADDR_Success fails halfway
# and leaves lo with another address, so lo's netmask is no longer the one this test expects
SKIP="$SKIP:IoctlTest.EVIOCGNAME_*:IoctlTest.SIOCGIFNETMASK_Success"
# Hangs on both sides in this case until the 60 s timeout, although it passes when terminal_test
# runs alone in the same image (cause not found): only the timeout would be compared
SKIP="$SKIP:Pty.SendSignals"
sh /tmp/difftest-files/gtest/run.sh starnix "$SKIP" epoll_test poll_test timerfd_test getrandom_test \
    ioctl_test terminal_test netlink_test
